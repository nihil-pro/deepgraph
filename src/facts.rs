#[derive(Debug, Clone)]
pub enum DepRef {
    Internal(String), // path relative to the scanned root
    External(String), // raw, unresolved specifier
}

/// Which names of a dependency a given edge actually needs. Used to
/// follow re-exports through a barrel file by the *specific* name that
/// was imported, instead of fanning out to everything the barrel
/// re-exports (see `name_sources`/`wildcard_fallbacks` below).
#[derive(Debug, Clone)]
pub enum ImportWant {
    /// Can't narrow: a namespace import (`import * as ns`), a
    /// side-effect import, `require(...)`, or a dynamic `import(...)`.
    All,
    /// The specific names as exported by the *target* module (i.e.
    /// pre-`as`-aliasing on the importer's side).
    Named(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct Dependency {
    pub target: DepRef,
    pub want: ImportWant,
}

/// What a single source file "does" as far as the dependency graph is
/// concerned, already resolved to concrete files where possible.
#[derive(Debug, Clone, Default)]
pub struct FileFacts {
    pub dependencies: Vec<Dependency>,
    pub exports: Vec<String>,
    pub has_local_exports: bool,
    pub has_reexports: bool,
    /// Externally-visible re-exported name -> where it actually comes
    /// from. JS/TS only (always empty for Python/Java). Populated from
    /// `export { x } from`/`export * as ns from` clauses, where the
    /// name is known at extraction time; `export * from` targets are
    /// unknown until `graph.rs` cross-references the target's own
    /// `exports` once every file has been parsed, so those go in
    /// `wildcard_fallbacks` instead.
    pub name_sources: Vec<(String, DepRef)>,
    /// `export * from` targets whose own export names aren't (fully)
    /// known at extraction time, so a name that doesn't match
    /// `name_sources` might still come from one of these.
    pub wildcard_fallbacks: Vec<DepRef>,
}
