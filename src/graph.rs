use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;

use tree_sitter::{Parser, Tree};

use crate::facts::{DepRef, FileFacts};
use crate::lang::{java, js_pkg, js_ts, python, ts_paths};
use crate::model::{Graph, Node};
use crate::walk::{Lang, SourceFile};

pub struct GraphOptions {
    /// Keep `external_dependencies` on each node. Off by default: most
    /// consumers only care about files inside the scanned directory.
    pub include_external: bool,
    /// Keep barrel files (pure re-export files, e.g. `index.ts`) as
    /// nodes. Off by default: they're dropped entirely and every
    /// remaining node's `dependencies`/`dependents` are rewired to skip
    /// straight through to real files (i.e. what `effective_dependencies`/
    /// `effective_dependents` would otherwise have been).
    pub include_barrels: bool,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            include_external: false,
            include_barrels: false,
        }
    }
}

struct BuildNode {
    hash: String,
    exports: Vec<String>,
    is_barrel: bool,
    dependencies: BTreeSet<String>,
    external_dependencies: BTreeSet<String>,
}

fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn split_deps(deps: Vec<DepRef>, self_rel: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut internal = BTreeSet::new();
    let mut external = BTreeSet::new();
    for d in deps {
        match d {
            DepRef::Internal(p) => {
                if p != self_rel {
                    internal.insert(p);
                }
            }
            DepRef::External(s) => {
                external.insert(s);
            }
        }
    }
    (internal, external)
}

pub fn build_graph(root: &Path, files: &[SourceFile], opts: &GraphOptions) -> anyhow::Result<Graph> {
    let mut js_parser = Parser::new();
    js_parser.set_language(&tree_sitter_javascript::language())?;
    let mut ts_parser = Parser::new();
    ts_parser.set_language(&tree_sitter_typescript::language_typescript())?;
    let mut tsx_parser = Parser::new();
    tsx_parser.set_language(&tree_sitter_typescript::language_tsx())?;
    let mut py_parser = Parser::new();
    py_parser.set_language(&tree_sitter_python::language())?;
    let mut java_parser = Parser::new();
    java_parser.set_language(&tree_sitter_java::language())?;

    let js_pkg_index = js_pkg::scan_packages(root);
    let ts_paths_index = ts_paths::scan(root);

    // --- Java needs a whole-project index before any file's imports can
    // be resolved, so parse all Java files up front and keep the trees
    // around for a second pass. ---
    struct ParsedJava {
        rel: String,
        src: String,
        tree: Tree,
        header: java::FileHeader,
    }
    let mut parsed_java: Vec<ParsedJava> = Vec::new();
    let mut java_index = java::JavaIndex::default();

    for f in files.iter().filter(|f| f.lang == Lang::Java) {
        let src = std::fs::read_to_string(&f.abs_path)?;
        let Some(tree) = java_parser.parse(&src, None) else {
            continue;
        };
        let header = java::scan_header(&tree, src.as_bytes());
        java_index.add(&f.rel_path, &header);
        parsed_java.push(ParsedJava {
            rel: f.rel_path.clone(),
            src,
            tree,
            header,
        });
    }

    let mut build_nodes: BTreeMap<String, BuildNode> = BTreeMap::new();

    for pj in &parsed_java {
        let imports = java::scan_imports(&pj.tree, pj.src.as_bytes());
        let facts = java::resolve(&imports, &pj.header.types, &pj.rel, &java_index);
        let (internal, external) = split_deps(facts.dependencies, &pj.rel);
        build_nodes.insert(
            pj.rel.clone(),
            BuildNode {
                hash: content_hash(pj.src.as_bytes()),
                exports: facts.exports,
                is_barrel: false,
                dependencies: internal,
                external_dependencies: external,
            },
        );
    }

    for f in files.iter().filter(|f| f.lang != Lang::Java) {
        let src = std::fs::read_to_string(&f.abs_path)?;
        let importer_dir = f
            .abs_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| root.to_path_buf());

        let facts: FileFacts = match f.lang {
            Lang::JavaScript => {
                let Some(tree) = js_parser.parse(&src, None) else {
                    continue;
                };
                js_ts::analyze(&tree, src.as_bytes(), root, &importer_dir, &js_pkg_index, &ts_paths_index)
            }
            Lang::TypeScript => {
                let is_tsx = f
                    .abs_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("tsx"))
                    .unwrap_or(false);
                let parser = if is_tsx { &mut tsx_parser } else { &mut ts_parser };
                let Some(tree) = parser.parse(&src, None) else {
                    continue;
                };
                js_ts::analyze(&tree, src.as_bytes(), root, &importer_dir, &js_pkg_index, &ts_paths_index)
            }
            Lang::Python => {
                let Some(tree) = py_parser.parse(&src, None) else {
                    continue;
                };
                python::analyze(&tree, src.as_bytes(), root, &importer_dir, &f.rel_path)
            }
            Lang::Java => unreachable!(),
        };

        let is_barrel = facts.has_reexports && !facts.has_local_exports;
        let (internal, external) = split_deps(facts.dependencies, &f.rel_path);
        build_nodes.insert(
            f.rel_path.clone(),
            BuildNode {
                hash: content_hash(src.as_bytes()),
                exports: facts.exports,
                is_barrel,
                dependencies: internal,
                external_dependencies: external,
            },
        );
    }

    // Reverse edges.
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (rel, node) in &build_nodes {
        for dep in &node.dependencies {
            dependents.entry(dep.clone()).or_default().insert(rel.clone());
        }
    }

    // Effective dependencies: follow barrel files through to their
    // underlying non-barrel sources. File-level, not per-symbol, so
    // importing anything from a barrel is treated as depending on
    // everything it re-exports.
    let mut effective_dependencies: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (rel, node) in &build_nodes {
        let mut result = BTreeSet::new();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = node.dependencies.iter().cloned().collect();
        while let Some(d) = queue.pop_front() {
            if d == *rel || !visited.insert(d.clone()) {
                continue;
            }
            match build_nodes.get(&d) {
                Some(dep_node) if dep_node.is_barrel => {
                    for next in &dep_node.dependencies {
                        queue.push_back(next.clone());
                    }
                }
                _ => {
                    result.insert(d);
                }
            }
        }
        effective_dependencies.insert(rel.clone(), result);
    }

    let mut effective_dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (rel, deps) in &effective_dependencies {
        for dep in deps {
            effective_dependents
                .entry(dep.clone())
                .or_default()
                .insert(rel.clone());
        }
    }

    let mut nodes = BTreeMap::new();
    for (rel, bn) in build_nodes {
        nodes.insert(
            rel.clone(),
            Node {
                hash: bn.hash,
                is_barrel: Some(bn.is_barrel),
                exports: bn.exports,
                dependencies: bn.dependencies.into_iter().collect(),
                dependents: dependents.remove(&rel).map(|s| s.into_iter().collect()).unwrap_or_default(),
                effective_dependencies: Some(
                    effective_dependencies
                        .remove(&rel)
                        .map(|s| s.into_iter().collect())
                        .unwrap_or_default(),
                ),
                effective_dependents: Some(
                    effective_dependents
                        .remove(&rel)
                        .map(|s| s.into_iter().collect())
                        .unwrap_or_default(),
                ),
                external_dependencies: Some(bn.external_dependencies.into_iter().collect()),
            },
        );
    }

    if !opts.include_barrels {
        let barrel_rels: BTreeSet<String> = nodes
            .iter()
            .filter(|(_, n)| n.is_barrel == Some(true))
            .map(|(rel, _)| rel.clone())
            .collect();
        nodes.retain(|rel, _| !barrel_rels.contains(rel));
        for node in nodes.values_mut() {
            // `effective_dependencies` never contains a barrel (the BFS
            // that built it skips through them), but a barrel's own
            // *effective_dependents* entry can still name a barrel: a
            // barrel legitimately "depends on" what it re-exports, so
            // it shows up as a dependent of that file in full mode.
            // With barrels deleted, that reference is now dangling.
            node.dependencies = node.effective_dependencies.take().unwrap_or_default();
            node.dependents = node
                .effective_dependents
                .take()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| !barrel_rels.contains(r))
                .collect();
            node.is_barrel = None;
        }
    }

    if !opts.include_external {
        for node in nodes.values_mut() {
            node.external_dependencies = None;
        }
    }

    Ok(Graph {
        root: root.to_string_lossy().to_string(),
        generated_at: crate::time_fmt::now_iso8601(),
        nodes,
    })
}
