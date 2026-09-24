use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    Java,
    Python,
    JavaScript,
    TypeScript,
}

/// Directory names that are always skipped, regardless of .gitignore,
/// because they never contain hand-authored source we want in the graph.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "out",
    "target",
    ".next",
    ".nuxt",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "coverage",
    "vendor",
    ".idea",
    ".vscode",
    ".gradle",
    ".settings",
];

pub fn detect_lang(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "java" => Some(Lang::Java),
        "py" | "pyi" => Some(Lang::Python),
        "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
        "ts" | "mts" | "cts" | "tsx" => Some(Lang::TypeScript),
        _ => None,
    }
}

pub struct SourceFile {
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub lang: Lang,
}

pub fn collect_source_files(root: &Path) -> anyhow::Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    return !SKIP_DIRS.contains(&name);
                }
            }
            true
        })
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let abs_path = entry.path().to_path_buf();
        let Some(lang) = detect_lang(&abs_path) else {
            continue;
        };
        let rel_path = abs_path
            .strip_prefix(root)
            .unwrap_or(&abs_path)
            .to_string_lossy()
            .replace('\\', "/");
        files.push(SourceFile {
            abs_path,
            rel_path,
            lang,
        });
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}
