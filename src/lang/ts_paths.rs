use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::tsutil::resolve_js_like_file;

struct Resolved {
    base_dir: PathBuf,
    has_base_url: bool,
    paths: HashMap<String, Vec<String>>,
}

struct TsConfig {
    /// Directory the config governs (its own directory); the nearest
    /// enclosing one wins for a given importer file.
    dir: PathBuf,
    resolved: Resolved,
}

#[derive(Default)]
pub struct TsPathsIndex {
    configs: Vec<TsConfig>,
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules"];

pub fn scan(root: &Path) -> TsPathsIndex {
    let mut idx = TsPathsIndex::default();
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
        let name = entry.file_name().to_str().unwrap_or("");
        if name != "tsconfig.json" && name != "jsconfig.json" {
            continue;
        }
        let path = entry.path();
        let Some(dir) = path.parent() else { continue };
        let resolved = load(path, 0);
        if resolved.paths.is_empty() && !resolved.has_base_url {
            continue; // nothing this config would change
        }
        idx.configs.push(TsConfig {
            dir: dir.to_path_buf(),
            resolved,
        });
    }

    idx
}

fn load(config_path: &Path, depth: u8) -> Resolved {
    let own_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut resolved = Resolved {
        base_dir: own_dir.clone(),
        has_base_url: false,
        paths: HashMap::new(),
    };

    if depth > 10 {
        return resolved;
    }

    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return resolved;
    };
    let cleaned = strip_trailing_commas(&strip_jsonc_comments(&raw));
    let Ok(json) = serde_json::from_str::<Value>(&cleaned) else {
        return resolved;
    };

    if let Some(extends) = json.get("extends").and_then(|v| v.as_str()) {
        if let Some(parent_path) = resolve_extends(&own_dir, extends) {
            resolved = load(&parent_path, depth + 1);
        }
    }

    if let Some(co) = json.get("compilerOptions") {
        if let Some(bu) = co.get("baseUrl").and_then(|v| v.as_str()) {
            resolved.base_dir = own_dir.join(bu);
            resolved.has_base_url = true;
        }
        if let Some(paths_val) = co.get("paths").and_then(|v| v.as_object()) {
            let mut map = HashMap::new();
            for (k, v) in paths_val {
                if let Some(arr) = v.as_array() {
                    let targets: Vec<String> =
                        arr.iter().filter_map(|t| t.as_str().map(str::to_string)).collect();
                    map.insert(k.clone(), targets);
                }
            }
            resolved.paths = map;
        }
    }

    resolved
}

fn resolve_extends(config_dir: &Path, extends: &str) -> Option<PathBuf> {
    if extends.starts_with('.') || extends.starts_with('/') {
        let p = config_dir.join(extends);
        if p.is_file() {
            return Some(p);
        }
        if p.extension().is_none() {
            let with_ext = PathBuf::from(format!("{}.json", p.to_string_lossy()));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
        return None;
    }
    // Non-relative: search ancestor node_modules, same as Node resolution.
    let mut cur = config_dir.to_path_buf();
    loop {
        let candidate = cur.join("node_modules").join(extends);
        if candidate.is_file() {
            return Some(candidate);
        }
        let as_pkg_default = candidate.join("tsconfig.json");
        if as_pkg_default.is_file() {
            return Some(as_pkg_default);
        }
        if candidate.extension().is_none() {
            let with_ext = PathBuf::from(format!("{}.json", candidate.to_string_lossy()));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Finds the pattern with the best match for `spec`: an exact
/// (non-wildcard) match wins outright, otherwise the wildcard pattern
/// with the longest literal prefix wins (mirrors tsc's own tie-break).
fn best_match<'a>(
    paths: &'a HashMap<String, Vec<String>>,
    spec: &str,
) -> Option<(&'a [String], String)> {
    let mut best: Option<(bool, usize, &[String], String)> = None;
    for (pattern, targets) in paths {
        if let Some(star) = pattern.find('*') {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            if spec.starts_with(prefix)
                && spec.ends_with(suffix)
                && spec.len() >= prefix.len() + suffix.len()
            {
                let captured = spec[prefix.len()..spec.len() - suffix.len()].to_string();
                let better = best.as_ref().map(|(exact, len, ..)| !exact && prefix.len() > *len).unwrap_or(true);
                if better {
                    best = Some((false, prefix.len(), targets, captured));
                }
            }
        } else if pattern == spec {
            best = Some((true, pattern.len(), targets, String::new()));
        }
    }
    best.map(|(_, _, targets, captured)| (targets, captured))
}

pub fn resolve(idx: &TsPathsIndex, importer_dir: &Path, spec: &str) -> Option<PathBuf> {
    let config = idx
        .configs
        .iter()
        .filter(|c| importer_dir.starts_with(&c.dir))
        .max_by_key(|c| c.dir.as_os_str().len())?;

    if let Some((targets, captured)) = best_match(&config.resolved.paths, spec) {
        for target in targets {
            let substituted = target.replace('*', &captured);
            if let Some(f) = resolve_js_like_file(&config.resolved.base_dir.join(substituted)) {
                return Some(f);
            }
        }
        return None;
    }

    if config.resolved.has_base_url {
        return resolve_js_like_file(&config.resolved.base_dir.join(spec));
    }

    None
}

/// Strips `//` and `/* */` comments from JSONC, respecting string literals.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(nc) = chars.next() {
                    out.push(nc);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    for nc in chars.by_ref() {
                        if nc == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for nc in chars.by_ref() {
                        if prev == '*' && nc == '/' {
                            break;
                        }
                        prev = nc;
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Removes trailing commas before `}`/`]`, respecting string literals.
fn strip_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}
