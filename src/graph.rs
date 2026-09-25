use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;

use tree_sitter::{Parser, Tree};

use crate::facts::{DepRef, Dependency, FileFacts, ImportWant};
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
    /// Flattened internal targets -- the raw, file-level view.
    dependencies: BTreeSet<String>,
    external_dependencies: BTreeSet<String>,
    /// Same internal targets as `dependencies`, but paired with which
    /// names each edge actually needs from the target. Used to follow
    /// barrel re-exports by the specific name that was imported, rather
    /// than fanning out to everything the barrel re-exports.
    dependency_wants: Vec<(String, ImportWant)>,
    /// Re-exported name -> where it really comes from (JS/TS only).
    name_sources: HashMap<String, DepRef>,
    /// `export * from` targets whose own surface isn't fully known, so
    /// an otherwise-unmatched name might still come from one of these.
    wildcard_fallbacks: Vec<DepRef>,
}

fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn split_deps(
    deps: Vec<Dependency>,
    self_rel: &str,
) -> (BTreeSet<String>, BTreeSet<String>, Vec<(String, ImportWant)>) {
    let mut internal = BTreeSet::new();
    let mut external = BTreeSet::new();
    let mut wants = Vec::new();
    for d in deps {
        match d.target {
            DepRef::Internal(p) => {
                if p != self_rel {
                    internal.insert(p.clone());
                    wants.push((p, d.want));
                }
            }
            DepRef::External(s) => {
                external.insert(s);
            }
        }
    }
    (internal, external, wants)
}

fn make_build_node(
    hash: String,
    facts: FileFacts,
    self_rel: &str,
    is_barrel: bool,
) -> BuildNode {
    let (internal, external, dependency_wants) = split_deps(facts.dependencies, self_rel);
    BuildNode {
        hash,
        exports: facts.exports,
        is_barrel,
        dependencies: internal,
        external_dependencies: external,
        dependency_wants,
        name_sources: facts.name_sources.into_iter().collect(),
        wildcard_fallbacks: facts.wildcard_fallbacks,
    }
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
        let hash = content_hash(pj.src.as_bytes());
        build_nodes.insert(pj.rel.clone(), make_build_node(hash, facts, &pj.rel, false));
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
        let hash = content_hash(src.as_bytes());
        build_nodes.insert(f.rel_path.clone(), make_build_node(hash, facts, &f.rel_path, is_barrel));
    }

    // A resolved internal path might not actually be a node: it could
    // have been excluded (--exclude), or failed to parse. Drop any such
    // dangling references before computing reverse edges so they don't
    // show up as dependencies/dependents pointing at nothing.
    let known: BTreeSet<String> = build_nodes.keys().cloned().collect();
    for node in build_nodes.values_mut() {
        node.dependencies.retain(|d| known.contains(d));
        node.dependency_wants.retain(|(d, _)| known.contains(d));
        node.wildcard_fallbacks.retain(|d| match d {
            DepRef::Internal(p) => known.contains(p),
            DepRef::External(_) => true,
        });
        node.name_sources.retain(|_, d| match d {
            DepRef::Internal(p) => known.contains(p),
            DepRef::External(_) => true,
        });
    }

    // Refine `export * from` wildcard fallbacks: once every file's own
    // `exports` are known, a wildcard target whose surface is fully
    // enumerable (no "*" of its own) has its names promoted into
    // `name_sources` for precise per-name resolution, and is dropped
    // from `wildcard_fallbacks` since it no longer needs to be guessed
    // at. Targets that are themselves not fully known (e.g. `module.exports
    // = {...}`, or a further unresolved wildcard) stay as fallbacks.
    let refinements: Vec<(String, Vec<(String, DepRef)>, Vec<DepRef>)> = build_nodes
        .iter()
        .map(|(rel, node)| {
            let mut additions = Vec::new();
            let mut kept_fallbacks = Vec::new();
            for fb in &node.wildcard_fallbacks {
                match fb {
                    DepRef::Internal(target_path) => match build_nodes.get(target_path) {
                        Some(target_node) => {
                            let fully_known = !target_node.exports.iter().any(|n| n == "*");
                            for name in &target_node.exports {
                                if name != "*" {
                                    additions.push((name.clone(), fb.clone()));
                                }
                            }
                            if !fully_known {
                                kept_fallbacks.push(fb.clone());
                            }
                        }
                        None => kept_fallbacks.push(fb.clone()),
                    },
                    DepRef::External(_) => kept_fallbacks.push(fb.clone()),
                }
            }
            (rel.clone(), additions, kept_fallbacks)
        })
        .collect();
    for (rel, additions, kept_fallbacks) in refinements {
        if let Some(node) = build_nodes.get_mut(&rel) {
            for (name, dep) in additions {
                node.name_sources.entry(name).or_insert(dep);
            }
            node.wildcard_fallbacks = kept_fallbacks;
        }
    }

    // Reverse edges (raw, file-level).
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (rel, node) in &build_nodes {
        for dep in &node.dependencies {
            dependents.entry(dep.clone()).or_default().insert(rel.clone());
        }
    }

    // Effective dependencies: follow barrel files through to their
    // underlying non-barrel sources, tracking *which name* is being
    // chased so `import { FOO } from 'barrel'` and `import { BAZ } from
    // 'barrel'` land on the specific files that actually provide FOO and
    // BAZ, instead of both fanning out to everything the barrel
    // re-exports. Falls back to expanding everything for edges we can't
    // narrow (namespace imports, `require`, or a name that doesn't match
    // any known re-export -- fail safe rather than under-report).
    #[derive(Clone, PartialEq, Eq, Hash)]
    enum Want {
        All,
        Name(String),
    }

    fn seed_wants(w: &ImportWant) -> Vec<Want> {
        match w {
            ImportWant::All => vec![Want::All],
            ImportWant::Named(names) if names.is_empty() => vec![Want::All],
            ImportWant::Named(names) => names.iter().cloned().map(Want::Name).collect(),
        }
    }

    let mut effective_dependencies: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (rel, node) in &build_nodes {
        let mut result: BTreeSet<String> = BTreeSet::new();
        let mut visited: HashSet<(String, Want)> = HashSet::new();
        let mut queue: VecDeque<(String, Want)> = VecDeque::new();
        for (target, want) in &node.dependency_wants {
            for w in seed_wants(want) {
                queue.push_back((target.clone(), w));
            }
        }

        while let Some((path, want)) = queue.pop_front() {
            if path == *rel || !visited.insert((path.clone(), want.clone())) {
                continue;
            }
            let Some(target_node) = build_nodes.get(&path) else {
                continue;
            };
            if !target_node.is_barrel {
                result.insert(path);
                continue;
            }
            match &want {
                Want::All => {
                    for (t, _) in &target_node.dependency_wants {
                        queue.push_back((t.clone(), Want::All));
                    }
                }
                Want::Name(name) => match target_node.name_sources.get(name) {
                    Some(DepRef::Internal(p)) => queue.push_back((p.clone(), Want::Name(name.clone()))),
                    Some(DepRef::External(_)) => {
                        // Re-exported from a third-party package; not
                        // representable in `effective_dependencies`
                        // (internal-only), so it's dropped here. Still
                        // visible on the barrel's own node if kept via
                        // `--with-barrels`.
                    }
                    None if !target_node.wildcard_fallbacks.is_empty() => {
                        for fb in &target_node.wildcard_fallbacks {
                            if let DepRef::Internal(p) = fb {
                                queue.push_back((p.clone(), Want::Name(name.clone())));
                            }
                        }
                    }
                    None => {
                        for (t, _) in &target_node.dependency_wants {
                            queue.push_back((t.clone(), Want::All));
                        }
                    }
                },
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
