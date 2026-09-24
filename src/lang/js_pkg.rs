use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::tsutil::resolve_js_like_file;

pub struct PackageInfo {
    pub root: PathBuf,
    pub main: Option<String>,
    pub exports: Option<Value>,
}

/// Maps npm/yarn/pnpm workspace package names (from each `package.json`'s
/// `"name"` field) to where their entry point / subpaths live on disk, so
/// a bare specifier like `import { foo } from 'a'` can be resolved to a
/// local sibling package instead of treated as an external dependency.
#[derive(Default)]
pub struct JsPackageIndex {
    by_name: HashMap<String, PackageInfo>,
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules"];

pub fn scan_packages(root: &Path) -> JsPackageIndex {
    let mut idx = JsPackageIndex::default();
    let walker = ignore::WalkBuilder::new(root)
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

    for entry in walker.flatten() {
        if entry.file_name() != "package.json" {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let Some(name) = json.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(pkg_root) = path.parent() else {
            continue;
        };
        idx.by_name.insert(
            name.to_string(),
            PackageInfo {
                root: pkg_root.to_path_buf(),
                main: json.get("main").and_then(|v| v.as_str()).map(str::to_string),
                exports: json.get("exports").cloned(),
            },
        );
    }

    idx
}

/// Splits a bare specifier into (package name, optional subpath), aware
/// that scoped packages (`@scope/name`) contain a slash in the name.
fn split_specifier(spec: &str) -> (String, Option<String>) {
    if let Some(rest) = spec.strip_prefix('@') {
        let mut parts = rest.splitn(2, '/');
        let name_part = parts.next().unwrap_or("");
        let after_scope = parts.next();
        match after_scope {
            None => (format!("@{name_part}"), None),
            Some(after_scope) => {
                let mut parts2 = after_scope.splitn(2, '/');
                let pkg = parts2.next().unwrap_or("");
                let sub = parts2.next();
                (format!("@{name_part}/{pkg}"), sub.map(str::to_string))
            }
        }
    } else {
        let mut parts = spec.splitn(2, '/');
        let name = parts.next().unwrap_or("").to_string();
        let sub = parts.next().map(str::to_string);
        (name, sub)
    }
}

fn exports_lookup(exports: &Value, key: &str) -> Option<String> {
    match exports {
        Value::String(s) if key == "." => Some(s.clone()),
        Value::Object(map) => match map.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Object(cond) => {
                for k in ["types", "import", "module", "default", "require"] {
                    if let Some(Value::String(s)) = cond.get(k) {
                        return Some(s.clone());
                    }
                }
                None
            }
            _ => None,
        },
        _ => None,
    }
}

fn resolve_entry(info: &PackageInfo) -> Option<PathBuf> {
    if let Some(exports) = &info.exports {
        if let Some(p) = exports_lookup(exports, ".") {
            if let Some(f) = resolve_js_like_file(&info.root.join(p.trim_start_matches("./"))) {
                return Some(f);
            }
        }
    }
    if let Some(main) = &info.main {
        if let Some(f) = resolve_js_like_file(&info.root.join(main)) {
            return Some(f);
        }
    }
    if let Some(f) = resolve_js_like_file(&info.root) {
        return Some(f);
    }
    // Common unbuilt-monorepo-package layout: root has no index, but
    // `main`/`exports` point at a `dist/` build that doesn't exist yet.
    resolve_js_like_file(&info.root.join("src"))
}

fn resolve_subpath(info: &PackageInfo, sub: &str) -> Option<PathBuf> {
    if let Some(exports) = &info.exports {
        let key = format!("./{sub}");
        if let Some(p) = exports_lookup(exports, &key) {
            if let Some(f) = resolve_js_like_file(&info.root.join(p.trim_start_matches("./"))) {
                return Some(f);
            }
        }
    }
    resolve_js_like_file(&info.root.join(sub))
}

/// Resolves a bare specifier (e.g. `a`, `a/utils`, `@scope/a/utils`)
/// against known local workspace packages. Returns `None` if it doesn't
/// match any known package (true external, e.g. `react`), or matches one
/// but no entry file could be found on disk.
pub fn resolve_bare_specifier(idx: &JsPackageIndex, spec: &str) -> Option<PathBuf> {
    let (pkg_name, subpath) = split_specifier(spec);
    let info = idx.by_name.get(&pkg_name)?;
    match subpath {
        None => resolve_entry(info),
        Some(sub) => resolve_subpath(info, &sub),
    }
}
