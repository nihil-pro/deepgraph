use std::path::Path;

use tree_sitter::{Node, Tree};

use crate::facts::{DepRef, Dependency, FileFacts, ImportWant};
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
    let mut has_any_reexport = false;

    let root_node = tree.root_node();
    let mut stack = vec![root_node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "import_statement" => {
                handle_import_statement(node, src, root, importer_dir, pkg_index, ts_paths_index, &mut facts);
            }
            "export_statement" => {
                handle_export_statement(
                    node,
                    src,
                    root,
                    importer_dir,
                    pkg_index,
                    ts_paths_index,
                    &mut facts,
                    &mut has_any_reexport,
                );
            }
            "call_expression" => {
                if let Some(spec) = require_or_dynamic_import_source(node, src) {
                    let target = resolve_specifier(root, importer_dir, &spec, pkg_index, ts_paths_index);
                    facts.dependencies.push(Dependency { target, want: ImportWant::All });
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

    facts.has_reexports = has_any_reexport;
    facts.exports.sort();
    facts.exports.dedup();
    facts
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

/// What names an `import_clause` (the part between `import` and `from`)
/// actually binds, as seen from the *source* module's perspective (i.e.
/// each name's `name` field, not its local `alias`).
fn import_clause_want(clause: Node, src: &[u8]) -> ImportWant {
    let mut names = Vec::new();
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "identifier" => names.push("default".to_string()),
            "namespace_import" => return ImportWant::All,
            "named_imports" => {
                for spec_node in children_of_kind(child, "import_specifier") {
                    if let Some(name) = spec_node.child_by_field_name("name") {
                        names.push(node_text(name, src).to_string());
                    }
                }
            }
            _ => {}
        }
    }
    ImportWant::Named(names)
}

fn handle_import_statement(
    node: Node,
    src: &[u8],
    root: &Path,
    importer_dir: &Path,
    pkg_index: &JsPackageIndex,
    ts_paths_index: &TsPathsIndex,
    facts: &mut FileFacts,
) {
    let Some(source_node) = node.child_by_field_name("source") else {
        return;
    };
    let Some(spec) = string_literal_content(source_node, src) else {
        return;
    };
    let target = resolve_specifier(root, importer_dir, &spec, pkg_index, ts_paths_index);
    let want = match first_child_of_kind(node, "import_clause") {
        None => ImportWant::All, // side-effect import: `import 'mod'`
        Some(clause) => import_clause_want(clause, src),
    };
    facts.dependencies.push(Dependency { target, want });
}

fn handle_export_statement(
    node: Node,
    src: &[u8],
    root: &Path,
    importer_dir: &Path,
    pkg_index: &JsPackageIndex,
    ts_paths_index: &TsPathsIndex,
    facts: &mut FileFacts,
    has_any_reexport: &mut bool,
) {
    let source_spec = node
        .child_by_field_name("source")
        .and_then(|s| string_literal_content(s, src));

    if let Some(spec) = source_spec {
        *has_any_reexport = true;
        let target = resolve_specifier(root, importer_dir, &spec, pkg_index, ts_paths_index);

        if let Some(clause) = first_child_of_kind(node, "export_clause") {
            // `export { x, y as z } from './foo'`: each name's source in
            // `foo` is known exactly, and so is what it's called here.
            let mut wanted = Vec::new();
            for spec_node in children_of_kind(clause, "export_specifier") {
                let Some(orig) = spec_node.child_by_field_name("name") else {
                    continue;
                };
                let orig = node_text(orig, src).to_string();
                let external_name = spec_node
                    .child_by_field_name("alias")
                    .map(|a| node_text(a, src).to_string())
                    .unwrap_or_else(|| orig.clone());
                facts.exports.push(external_name.clone());
                facts.name_sources.push((external_name, target.clone()));
                wanted.push(orig);
            }
            facts.dependencies.push(Dependency { target, want: ImportWant::Named(wanted) });
        } else if let Some(ns) = first_child_of_kind(node, "namespace_export") {
            // `export * as ns from './foo'`: everything in foo is only
            // reachable through the single name `ns`.
            if let Some(ident) = first_named_child_of_kinds(ns, &["identifier"]) {
                let alias = node_text(ident, src).to_string();
                facts.exports.push(alias.clone());
                facts.name_sources.push((alias, target.clone()));
            }
            facts.dependencies.push(Dependency { target, want: ImportWant::All });
        } else {
            // `export * from './foo'`: which names this actually
            // provides depends on foo's own exports, not known until
            // graph.rs cross-references every file's `exports`.
            facts.exports.push("*".to_string());
            facts.wildcard_fallbacks.push(target.clone());
            facts.dependencies.push(Dependency { target, want: ImportWant::All });
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
