#[derive(Debug, Clone)]
pub enum DepRef {
    Internal(String), // path relative to the scanned root
    External(String), // raw, unresolved specifier
}

/// What a single source file "does" as far as the dependency graph is
/// concerned, already resolved to concrete files where possible.
#[derive(Debug, Clone, Default)]
pub struct FileFacts {
    pub dependencies: Vec<DepRef>,
    pub exports: Vec<String>,
    pub has_local_exports: bool,
    pub has_reexports: bool,
}
