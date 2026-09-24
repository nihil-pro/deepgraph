use std::path::{Path, PathBuf};

use tree_sitter::{Node, Tree};

use crate::facts::{DepRef, FileFacts};
use crate::tsutil::*;

pub fn analyze(tree: &Tree, src: &[u8], root: &Path, importer_dir: &Path, file_rel: &str) -> FileFacts {
    let mut facts = FileFacts::default();
    let root_node = tree.root_node();

    // Imports can appear anywhere (top level, try/except, TYPE_CHECKING
    // guards, function bodies) so walk the whole tree for them.
    let mut deps: Vec<DepRef> = Vec::new();
    let mut stack = vec![root_node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "import_statement" => handle_import_statement(node, src, root, &mut deps),
            "import_from_statement" => {
                handle_import_from_statement(node, src, root, importer_dir, &mut deps)
            }
            _ => {}
        }
        for i in 0..node.child_count() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
    }
    facts.dependencies = deps;

    // Only a module's own top level defines its public surface.
    let mut all_list: Option<Vec<String>> = None;
    let mut top_names: Vec<String> = Vec::new();
    let mut top_level_has_import = false;
    let mut cursor = root_node.walk();
    for child in root_node.children(&mut cursor) {
        match child.kind() {
            "import_statement" | "import_from_statement" => top_level_has_import = true,
            "function_definition" | "class_definition" => {
                if let Some(name) = child.child_by_field_name("name") {
                    top_names.push(node_text(name, src).to_string());
                }
            }
            "expression_statement" => {
                if let Some(assign) = first_child_of_kind(child, "assignment") {
                    if let Some(left) = assign.child_by_field_name("left") {
                        if left.kind() == "identifier" {
                            let name = node_text(left, src).to_string();
                            if name == "__all__" {
                                if let Some(right) = assign.child_by_field_name("right") {
                                    all_list = Some(extract_string_list(right, src));
                                }
                            } else if !name.starts_with('_') {
                                top_names.push(name);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(names) = all_list {
        facts.has_local_exports = !names.is_empty();
        facts.exports = names;
    } else {
        facts.has_local_exports = !top_names.is_empty();
        facts.exports = top_names;
    }

    let is_init = file_rel.ends_with("__init__.py") || file_rel == "__init__.py";
    facts.has_reexports = is_init && top_level_has_import;

    facts
}

fn extract_string_list(node: Node, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if node.kind() != "list" && node.kind() != "tuple" {
        return out;
    }
    for child in children_of_kind(node, "string") {
        if let Some(s) = string_literal_content(child, src) {
            out.push(s);
        }
    }
    out
}

fn dotted_name_text(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "dotted_name" => Some(node_text(node, src).to_string()),
        "aliased_import" => {
            let inner = node.child_by_field_name("name")?;
            dotted_name_text(inner, src)
        }
        _ => None,
    }
}

fn handle_import_statement(node: Node, src: &[u8], root: &Path, deps: &mut Vec<DepRef>) {
    let mut cursor = node.walk();
    for name_node in node.children_by_field_name("name", &mut cursor) {
        if let Some(dotted) = dotted_name_text(name_node, src) {
            deps.push(resolve_absolute_module(root, &dotted));
        }
    }
}

fn handle_import_from_statement(
    node: Node,
    src: &[u8],
    root: &Path,
    importer_dir: &Path,
    deps: &mut Vec<DepRef>,
) {
    let Some(module_node) = node.child_by_field_name("module_name") else {
        return;
    };

    let (base_dir, base_spec): (PathBuf, String) = match module_node.kind() {
        "dotted_name" => {
            let text = node_text(module_node, src).to_string();
            let rel = text.replace('.', "/");
            (root.join(rel), text)
        }
        "relative_import" => {
            let text = node_text(module_node, src).to_string();
            match resolve_relative_base_dir(importer_dir, &text) {
                Some(dir) => (dir, text),
                None => {
                    deps.push(DepRef::External(text));
                    return;
                }
            }
        }
        _ => return,
    };

    if first_child_of_kind(node, "wildcard_import").is_some() {
        deps.push(resolve_base_or_external(root, &base_dir, &base_spec));
        return;
    }

    let mut cursor = node.walk();
    let mut any = false;
    for name_node in node.children_by_field_name("name", &mut cursor) {
        let Some(name) = dotted_name_text(name_node, src) else {
            continue;
        };
        any = true;
        if let Some(sub) = resolve_submodule(root, &base_dir, &name) {
            deps.push(DepRef::Internal(sub));
        } else {
            match resolve_base_dir(root, &base_dir) {
                Some(p) => deps.push(DepRef::Internal(p)),
                None => deps.push(DepRef::External(format!("{base_spec}.{name}"))),
            }
        }
    }
    if !any {
        deps.push(resolve_base_or_external(root, &base_dir, &base_spec));
    }
}

fn resolve_relative_base_dir(importer_dir: &Path, rel_spec: &str) -> Option<PathBuf> {
    let dots = rel_spec.chars().take_while(|&c| c == '.').count();
    if dots == 0 {
        return None;
    }
    let rest = &rel_spec[dots..];
    let mut dir = importer_dir.to_path_buf();
    for _ in 0..dots.saturating_sub(1) {
        dir = dir.parent()?.to_path_buf();
    }
    if !rest.is_empty() {
        for seg in rest.split('.') {
            dir = dir.join(seg);
        }
    }
    Some(dir)
}

fn resolve_absolute_module(root: &Path, dotted: &str) -> DepRef {
    let rel = dotted.replace('.', "/");
    let dir_form = root.join(&rel);
    match resolve_base_dir(root, &dir_form) {
        Some(p) => DepRef::Internal(p),
        None => DepRef::External(dotted.to_string()),
    }
}

/// `base_dir` treated as a module: either `base_dir.py` or `base_dir/__init__.py`.
fn resolve_base_dir(root: &Path, base_dir: &Path) -> Option<String> {
    let as_pkg = base_dir.join("__init__.py");
    if as_pkg.is_file() {
        return canon_rel(root, &as_pkg);
    }
    let as_file = PathBuf::from(format!("{}.py", base_dir.to_string_lossy()));
    if as_file.is_file() {
        return canon_rel(root, &as_file);
    }
    None
}

fn resolve_submodule(root: &Path, base_dir: &Path, name: &str) -> Option<String> {
    let as_file = base_dir.join(format!("{name}.py"));
    if as_file.is_file() {
        return canon_rel(root, &as_file);
    }
    let as_pkg = base_dir.join(name).join("__init__.py");
    if as_pkg.is_file() {
        return canon_rel(root, &as_pkg);
    }
    None
}

fn resolve_base_or_external(root: &Path, base_dir: &Path, base_spec: &str) -> DepRef {
    match resolve_base_dir(root, base_dir) {
        Some(p) => DepRef::Internal(p),
        None => DepRef::External(base_spec.to_string()),
    }
}

fn canon_rel(root: &Path, path: &Path) -> Option<String> {
    let canon = path.canonicalize().ok()?;
    let rel = canon.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}
