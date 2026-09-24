use std::path::Path;

use tree_sitter::{Node, Tree};

use crate::facts::{DepRef, FileFacts};
use crate::lang::js_pkg::JsPackageIndex;
use crate::lang::ts_paths::TsPathsIndex;
use crate::tsutil::*;

pub fn analyze(
    tree: &Tree,
    src: &[u8],
    root: &Path,
    importer_dir: &Path,
    pkg_index: &JsPackageIndex,
    ts_paths_index: &TsPathsIndex,
) -> FileFacts {
    let mut facts = FileFacts::default();
    let mut raw_deps: Vec<String> = Vec::new(); // plain import/require specifiers
    let mut raw_reexports: Vec<String> = Vec::new(); // export ... from specifiers

    let root_node = tree.root_node();
    let mut stack = vec![root_node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "import_statement" => {
                if let Some(spec) = import_source(node, src) {
                    raw_deps.push(spec);
                }
            }
            "export_statement" => {
                handle_export_statement(node, src, &mut facts, &mut raw_deps, &mut raw_reexports);
            }
            "call_expression" => {
                if let Some(spec) = require_or_dynamic_import_source(node, src) {
                    raw_deps.push(spec);
                }
            }
            "assignment_expression" => {
                handle_commonjs_assignment(node, src, &mut facts);
            }
            _ => {}
        }
        for i in 0..node.child_count() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
    }

    facts.has_reexports = !raw_reexports.is_empty();

    let mut seen = std::collections::HashSet::new();
    for spec in raw_deps.into_iter().chain(raw_reexports.into_iter()) {
        if !seen.insert(spec.clone()) {
            continue;
        }
        facts
            .dependencies
            .push(resolve_specifier(root, importer_dir, &spec, pkg_index, ts_paths_index));
    }

    facts
}

fn import_source(node: Node, src: &[u8]) -> Option<String> {
    let source_node = node.child_by_field_name("source")?;
    string_literal_content(source_node, src)
}

fn require_or_dynamic_import_source(node: Node, src: &[u8]) -> Option<String> {
    let func = node.child_by_field_name("function")?;
    let is_require = func.kind() == "identifier" && node_text(func, src) == "require";
    let is_dynamic_import = func.kind() == "import";
    if !is_require && !is_dynamic_import {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if child.kind() == "string" {
            return string_literal_content(child, src);
        }
    }
    None
}

fn handle_commonjs_assignment(node: Node, src: &[u8], facts: &mut FileFacts) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    if left.kind() != "member_expression" {
        return;
    }
    let Some(obj) = left.child_by_field_name("object") else {
        return;
    };
    let Some(prop) = left.child_by_field_name("property") else {
        return;
    };
    let obj_text = node_text(obj, src);
    let prop_text = node_text(prop, src);
    if obj_text == "module" && prop_text == "exports" {
        facts.has_local_exports = true;
        facts.exports.push("*".to_string());
    } else if obj_text == "exports" {
        facts.has_local_exports = true;
        facts.exports.push(prop_text.to_string());
    }
}

fn handle_export_statement(
    node: Node,
    src: &[u8],
    facts: &mut FileFacts,
    raw_deps: &mut Vec<String>,
    raw_reexports: &mut Vec<String>,
) {
    let source_spec = node
        .child_by_field_name("source")
        .and_then(|s| string_literal_content(s, src));

    if let Some(spec) = source_spec {
        raw_reexports.push(spec);
        if let Some(clause) = first_child_of_kind(node, "export_clause") {
            for spec_node in children_of_kind(clause, "export_specifier") {
                if let Some(name) = spec_node
                    .child_by_field_name("alias")
                    .or_else(|| spec_node.child_by_field_name("name"))
                {
                    facts.exports.push(node_text(name, src).to_string());
                }
            }
        } else {
            // `export * from '...'` or `export * as ns from '...'`
            facts.exports.push("*".to_string());
        }
        return;
    }

    // No source => local export.
    if has_child_kind(node, "default") {
        facts.has_local_exports = true;
        facts.exports.push("default".to_string());
        // A default export can still reference a require()/import() in its
        // value; that's picked up by the generic recursive walk already.
        return;
    }

    if let Some(decl) = node.child_by_field_name("declaration") {
        facts.has_local_exports = true;
        collect_declared_names(decl, src, &mut facts.exports);
        return;
    }

    if let Some(clause) = first_child_of_kind(node, "export_clause") {
        facts.has_local_exports = true;
        for spec_node in children_of_kind(clause, "export_specifier") {
            if let Some(name) = spec_node
                .child_by_field_name("alias")
                .or_else(|| spec_node.child_by_field_name("name"))
            {
                facts.exports.push(node_text(name, src).to_string());
            }
        }
        let _ = raw_deps; // reserved: local `export { a }` has no module source
    }
}

fn collect_declared_names(node: Node, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(node_text(name, src).to_string());
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            for child in children_of_kind(node, "variable_declarator") {
                if let Some(name) = child.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        out.push(node_text(name, src).to_string());
                    } else {
                        out.push("*".to_string());
                    }
                }
            }
        }
        _ => out.push("*".to_string()),
    }
}

fn resolve_specifier(
    root: &Path,
    importer_dir: &Path,
    spec: &str,
    pkg_index: &JsPackageIndex,
    ts_paths_index: &TsPathsIndex,
) -> DepRef {
    let resolved = if spec.starts_with('.') || spec.starts_with('/') {
        let candidate = if spec.starts_with('/') {
            root.join(spec.trim_start_matches('/'))
        } else {
            importer_dir.join(spec)
        };
        resolve_js_like_file(&candidate)
    } else {
        crate::lang::ts_paths::resolve(ts_paths_index, importer_dir, spec)
            .or_else(|| crate::lang::js_pkg::resolve_bare_specifier(pkg_index, spec))
    };

    if let Some(resolved) = resolved {
        if let Ok(canon) = resolved.canonicalize() {
            if !is_under_node_modules(&canon) {
                if let Ok(rel) = canon.strip_prefix(root) {
                    return DepRef::Internal(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    DepRef::External(spec.to_string())
}
