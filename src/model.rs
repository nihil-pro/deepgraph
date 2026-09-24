use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Pure structural output (`<base>.graph.json`): a deterministic function
/// of the source tree, with no authored data in it. Always safe to
/// regenerate wholesale -- no merge logic needed. `language` is left out
/// since it's already implied by the path's extension.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Node {
    /// Content hash (non-cryptographic). Used by `show` to tell whether
    /// a note in the `ctx.json` sidecar is still fresh.
    pub hash: String,

    /// True if this file exists only to re-export symbols from other
    /// files (e.g. a JS/TS `index.ts` barrel, or a Python `__init__.py`
    /// that just does `from .x import y`). Omitted entirely (not just
    /// `false`) unless `--with-barrels` keeps barrel files in the graph,
    /// since every node is then guaranteed not to be one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_barrel: Option<bool>,

    /// Best-effort list of names this file exports/declares publicly.
    /// "*" means "exports everything, exact names not tracked" (e.g. a
    /// wildcard re-export, or a Python module with no `__all__`).
    #[serde(default)]
    pub exports: Vec<String>,

    /// Direct, file-level dependencies: files this file imports from or
    /// re-exports from. Sorted, relative to the scanned root. Barrel
    /// files are skipped through to real files unless `--with-barrels`.
    #[serde(default)]
    pub dependencies: Vec<String>,

    /// Reverse of `dependencies`: files that import/re-export from this
    /// file. Computed, not authored.
    #[serde(default)]
    pub dependents: Vec<String>,

    /// `dependencies` with any barrel files followed through to their
    /// underlying non-barrel source files. Only present under
    /// `--with-barrels` (otherwise `dependencies` already is this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_dependencies: Option<Vec<String>>,

    /// Reverse of `effective_dependencies`. Same condition as above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_dependents: Option<Vec<String>>,

    /// Import specifiers that could not be resolved to a file inside the
    /// scanned root (external packages, stdlib modules, etc). Only
    /// present under `--with-external`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_dependencies: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Graph {
    pub root: String,
    pub generated_at: String,
    pub nodes: BTreeMap<String, Node>,
}

/// One entry in the `ctx.json` sidecar (`<base>.ctx.json`). This file is
/// never written by `depgraph build` -- it's meant to be authored by a
/// human or an LLM (see `depgraph show`) and only holds files that have
/// actually been annotated, so it stays small regardless of repo size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtxEntry {
    /// What the file does, not how. Free text.
    pub ctx: String,
    /// The file's `hash` (from `graph.json`) at the time `ctx` was
    /// written. `depgraph show` compares this against the current hash
    /// to report whether the note might be stale; update this field (or
    /// re-run whatever wrote `ctx`) to clear that.
    pub ctx_hash: String,
}

pub type CtxStore = BTreeMap<String, CtxEntry>;
