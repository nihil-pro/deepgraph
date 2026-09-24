use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::facts::{DepRef, FileFacts};
use crate::tsutil::*;

const TOP_LEVEL_TYPE_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "annotation_type_declaration",
];

pub struct FileHeader {
    pub package: String, // "" for default package
    pub types: Vec<String>,
}

pub struct RawImport {
    pub path: String,
    pub is_static: bool,
    pub is_wildcard: bool,
}

/// Global project index built from a first pass over every Java file,
/// needed before any single file's imports can be resolved.
#[derive(Default)]
pub struct JavaIndex {
    /// "pkg.Type" (or just "Type" for the default package) -> file rel path
    pub type_index: HashMap<String, String>,
    /// "pkg" -> rel paths of files declared in that package
    pub package_index: HashMap<String, Vec<String>>,
}

impl JavaIndex {
    pub fn add(&mut self, file_rel: &str, header: &FileHeader) {
        for ty in &header.types {
            let fqn = if header.package.is_empty() {
                ty.clone()
            } else {
                format!("{}.{}", header.package, ty)
            };
            self.type_index.insert(fqn, file_rel.to_string());
        }
        self.package_index
            .entry(header.package.clone())
            .or_default()
            .push(file_rel.to_string());
    }
}

pub fn scan_header(tree: &Tree, src: &[u8]) -> FileHeader {
    let root = tree.root_node();
    let mut package = String::new();
    let mut types = Vec::new();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "package_declaration" => {
                if let Some(ident) =
                    first_named_child_of_kinds(child, &["identifier", "scoped_identifier"])
                {
                    package = flatten_ident(ident, src);
                }
            }
            k if TOP_LEVEL_TYPE_KINDS.contains(&k) => {
                if let Some(name) = child.child_by_field_name("name") {
                    types.push(node_text(name, src).to_string());
                }
            }
            _ => {}
        }
    }

    FileHeader { package, types }
}

pub fn scan_imports(tree: &Tree, src: &[u8]) -> Vec<RawImport> {
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let is_static = has_child_kind(child, "static");
        let is_wildcard = has_child_kind(child, "asterisk");
        let Some(ident) = first_named_child_of_kinds(child, &["identifier", "scoped_identifier"])
        else {
            continue;
        };
        out.push(RawImport {
            path: flatten_ident(ident, src),
            is_static,
            is_wildcard,
        });
    }
    out
}

fn flatten_ident(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "scoped_identifier" => {
            let scope = node
                .child_by_field_name("scope")
                .map(|n| flatten_ident(n, src))
                .unwrap_or_default();
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            if scope.is_empty() {
                name
            } else {
                format!("{scope}.{name}")
            }
        }
        _ => node_text(node, src).to_string(),
    }
}

pub fn resolve(
    imports: &[RawImport],
    types: &[String],
    self_rel: &str,
    index: &JavaIndex,
) -> FileFacts {
    let mut facts = FileFacts::default();
    facts.exports = types.to_vec();
    facts.has_local_exports = !types.is_empty();
    for imp in imports {
        match (imp.is_wildcard, imp.is_static) {
            (true, false) => {
                if let Some(files) = index.package_index.get(&imp.path) {
                    let mut any = false;
                    for f in files {
                        if f != self_rel {
                            facts.dependencies.push(DepRef::Internal(f.clone()));
                            any = true;
                        }
                    }
                    if !any {
                        facts
                            .dependencies
                            .push(DepRef::External(format!("{}.*", imp.path)));
                    }
                } else {
                    facts
                        .dependencies
                        .push(DepRef::External(format!("{}.*", imp.path)));
                }
            }
            (true, true) => {
                // `import static pkg.Type.*;` -- imp.path is the class FQN.
                match index.type_index.get(&imp.path) {
                    Some(f) => facts.dependencies.push(DepRef::Internal(f.clone())),
                    None => facts
                        .dependencies
                        .push(DepRef::External(format!("{}.*", imp.path))),
                }
            }
            (false, true) => {
                // `import static pkg.Type.member;` -- try stripping the
                // trailing member first, then fall back to the full path
                // (covers `import static pkg.Type;`, unusual but legal).
                if let Some(idx) = imp.path.rfind('.') {
                    let parent = &imp.path[..idx];
                    if let Some(f) = index.type_index.get(parent) {
                        facts.dependencies.push(DepRef::Internal(f.clone()));
                        continue;
                    }
                }
                match index.type_index.get(&imp.path) {
                    Some(f) => facts.dependencies.push(DepRef::Internal(f.clone())),
                    None => facts
                        .dependencies
                        .push(DepRef::External(imp.path.clone())),
                }
            }
            (false, false) => match index.type_index.get(&imp.path) {
                Some(f) => facts.dependencies.push(DepRef::Internal(f.clone())),
                None => facts
                    .dependencies
                    .push(DepRef::External(imp.path.clone())),
            },
        }
    }
    facts
}
