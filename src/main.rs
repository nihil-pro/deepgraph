mod facts;
mod graph;
mod lang;
mod model;
mod time_fmt;
mod tsutil;
mod walk;

use std::path::{Path, PathBuf};

use clap::{Parser as ClapParser, Subcommand};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;

use model::{CtxStore, Graph, Node};

/// Build a bidirectional import/export dependency graph for a Java,
/// Python, JavaScript, or TypeScript codebase.
#[derive(ClapParser, Debug)]
#[command(name = "depgraph", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Scan a directory and write `<output>.graph.json`. Never touches
    /// or needs `<output>.ctx.json` -- the graph is a pure function of
    /// the source tree, always safe to regenerate wholesale.
    Build {
        /// Directory to scan.
        dir: PathBuf,

        /// Base name for the output file(s), e.g. `depgraph` or
        /// `depgraph.json` both produce `depgraph.graph.json` (and,
        /// once you or an LLM annotate anything, `depgraph.ctx.json`).
        output: PathBuf,

        /// Emit compact JSON instead of pretty-printed.
        #[arg(long)]
        compact: bool,

        /// Keep `external_dependencies` (npm/stdlib/JDK packages etc.)
        /// on every node. Off by default.
        #[arg(long)]
        with_external: bool,

        /// Keep barrel files (pure re-export files, e.g. an `index.ts`
        /// that only does `export * from './x'`) as nodes, along with
        /// `is_barrel`/`effective_dependencies`/`effective_dependents`.
        /// Off by default: barrels are dropped and every remaining
        /// node's `dependencies`/`dependents` skip straight through to
        /// the real files that used to sit behind them.
        #[arg(long)]
        with_barrels: bool,

        /// Exclude files/directories matching this gitignore-style glob
        /// (relative to `dir`), e.g. `--exclude '**/*.test.ts'` or
        /// `--exclude 'legacy/**'`. Repeatable.
        #[arg(long = "exclude")]
        excludes: Vec<String>,
    },

    /// Print one file's node from `<output>.graph.json`, merged with its
    /// note from `<output>.ctx.json` if one exists. `file` can be the
    /// full relative path or just a unique trailing suffix of it (e.g.
    /// `Button.tsx` or `components/Button.tsx`).
    Show {
        /// The same base name given to `build`.
        output: PathBuf,

        /// Full relative path, or a unique trailing suffix of it.
        file: String,

        /// Emit compact JSON instead of pretty-printed.
        #[arg(long)]
        compact: bool,
    },
}

#[derive(Serialize)]
struct ShowOutput<'a> {
    path: &'a str,
    #[serde(flatten)]
    node: &'a Node,
    ctx: String,
    ctx_stale: bool,
}

/// `depgraph.json` or `depgraph` -> (`depgraph.graph.json`, `depgraph.ctx.json`).
fn derive_paths(base: &Path) -> (PathBuf, PathBuf) {
    let stem = if base.extension().map(|e| e == "json").unwrap_or(false) {
        base.with_extension("")
    } else {
        base.to_path_buf()
    };
    let stem = stem.to_string_lossy();
    (
        PathBuf::from(format!("{stem}.graph.json")),
        PathBuf::from(format!("{stem}.ctx.json")),
    )
}

fn build_excludes(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).map_err(|e| anyhow::anyhow!("invalid --exclude pattern {p:?}: {e}"))?);
    }
    Ok(builder.build()?)
}

fn run_build(
    dir: PathBuf,
    output: PathBuf,
    compact: bool,
    with_external: bool,
    with_barrels: bool,
    excludes: &[String],
) -> anyhow::Result<()> {
    let root = dir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot access directory {:?}: {e}", dir))?;

    let exclude_set = build_excludes(excludes)?;
    let files = walk::collect_source_files(&root, &exclude_set)?;
    eprintln!("depgraph: scanning {} source file(s) under {}", files.len(), root.display());

    let opts = graph::GraphOptions {
        include_external: with_external,
        include_barrels: with_barrels,
    };
    let graph = graph::build_graph(&root, &files, &opts)?;

    let (graph_path, ctx_path) = derive_paths(&output);
    let json = if compact {
        serde_json::to_string(&graph)?
    } else {
        serde_json::to_string_pretty(&graph)?
    };
    if let Some(parent) = graph_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&graph_path, json)?;

    eprintln!(
        "depgraph: wrote {} node(s) to {} (notes live separately in {}, once you have any)",
        graph.nodes.len(),
        graph_path.display(),
        ctx_path.display()
    );

    Ok(())
}

fn run_show(output: PathBuf, file: String, compact: bool) -> anyhow::Result<()> {
    let (graph_path, ctx_path) = derive_paths(&output);

    let graph_json = std::fs::read_to_string(&graph_path)
        .map_err(|e| anyhow::anyhow!("cannot read {:?}: {e}", graph_path))?;
    let graph: Graph = serde_json::from_str(&graph_json)?;

    let ctx_store: CtxStore = std::fs::read_to_string(&ctx_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let query_parts: Vec<&str> = file.trim_matches('/').split('/').collect();
    let matches: Vec<&String> = graph
        .nodes
        .keys()
        .filter(|k| {
            if k.as_str() == file {
                return true;
            }
            let key_parts: Vec<&str> = k.split('/').collect();
            key_parts.len() >= query_parts.len()
                && key_parts[key_parts.len() - query_parts.len()..] == query_parts[..]
        })
        .collect();

    let path = match matches.len() {
        0 => anyhow::bail!("no file in {:?} matches {:?}", graph_path, file),
        1 => matches[0].clone(),
        _ => {
            let exact = graph.nodes.keys().find(|k| k.as_str() == file);
            if let Some(p) = exact {
                p.clone()
            } else {
                let mut list = matches.iter().map(|s| s.as_str()).collect::<Vec<_>>();
                list.sort();
                anyhow::bail!(
                    "{:?} matches {} files, be more specific:\n  {}",
                    file,
                    list.len(),
                    list.join("\n  ")
                );
            }
        }
    };

    let node = &graph.nodes[&path];
    let (ctx, ctx_stale) = match ctx_store.get(&path) {
        Some(entry) => (entry.ctx.clone(), entry.ctx_hash != node.hash),
        None => (String::new(), false),
    };

    let out = ShowOutput {
        path: &path,
        node,
        ctx,
        ctx_stale,
    };

    let json = if compact {
        serde_json::to_string(&out)?
    } else {
        serde_json::to_string_pretty(&out)?
    };
    println!("{json}");

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            dir,
            output,
            compact,
            with_external,
            with_barrels,
            excludes,
        } => run_build(dir, output, compact, with_external, with_barrels, &excludes),
        Command::Show { output, file, compact } => run_show(output, file, compact),
    }
}
