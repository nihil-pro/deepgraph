use std::path::{Path, PathBuf};

use tree_sitter::Node;

const RESOLVE_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "json"];
const INDEX_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// True if any path component is literally `node_modules`. Anything
/// under one is third-party and must never be treated as an internal
/// dependency, even if some resolution path (a `tsconfig.json` `paths`
/// entry, a package's own `exports` map, etc.) points straight at it.
pub fn is_under_node_modules(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == "node_modules")
}

/// Resolves `candidate` to a concrete JS/TS file: as-is, with a resolvable
/// extension appended, or as a directory containing an `index.*`. Shared
/// by relative-import resolution, workspace-package resolution, and
/// tsconfig path-alias resolution. Never resolves into `node_modules`.
pub fn resolve_js_like_file(candidate: &Path) -> Option<PathBuf> {
    if is_under_node_modules(candidate) {
        return None;
    }
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    let base = candidate.to_string_lossy().to_string();
    for ext in RESOLVE_EXTS {
        let p = PathBuf::from(format!("{base}.{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    for ext in INDEX_EXTS {
        let p = candidate.join(format!("index.{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Extracts the literal contents of a JS/TS `string` node (child kind
/// `string_fragment`) or a Python `string` node (child kind
/// `string_content`).
pub fn string_literal_content(node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_fragment" || child.kind() == "string_content" {
            return Some(node_text(child, src).to_string());
        }
    }
    None
}

pub fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|c| c.kind() == kind);
    found
}

pub fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

pub fn first_named_child_of_kinds<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| kinds.contains(&c.kind()));
    found
}

pub fn children_of_kind<'a>(node: Node<'a>, kind: &'a str) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.kind() == kind)
        .collect()
}
