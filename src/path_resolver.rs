//! Path expansion (`%VAR%`), relative path resolution against the wrapper folder, and `PATH` lookup.

use std::env;
use std::path::{Path, PathBuf};

/// Expand `%VARIABLE%` placeholders in `input` using the current process environment
/// (case-insensitive variable name fallback to match Windows semantics).
pub fn expand_env_vars(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }

    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            let mut var_name = String::new();
            let mut closed = false;
            for next_ch in chars.by_ref() {
                if next_ch == '%' {
                    closed = true;
                    break;
                }
                var_name.push(next_ch);
            }

            if closed && !var_name.is_empty() {
                if let Some(val) = lookup_env_case_insensitive(&var_name) {
                    result.push_str(&val);
                } else {
                    result.push('%');
                    result.push_str(&var_name);
                    result.push('%');
                }
            } else if closed {
                result.push_str("%%");
            } else {
                result.push('%');
                result.push_str(&var_name);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

fn lookup_env_case_insensitive(name: &str) -> Option<String> {
    if let Ok(val) = env::var(name) {
        return Some(val);
    }
    for (k, v) in env::vars() {
        if k.eq_ignore_ascii_case(name) {
            return Some(v);
        }
    }
    None
}

/// Normalize path separators on non-Windows platforms when reading Windows-style relative paths
/// such as `"..\\apps\\storefront"` from `appsettings.json`.
fn normalize_separators(raw: &str) -> String {
    #[cfg(unix)]
    {
        raw.replace('\\', "/")
    }
    #[cfg(windows)]
    {
        raw.to_string()
    }
}

/// Check if a path string is rooted (either platform native root or Windows drive root `C:\`).
fn is_rooted_path(path_str: &str) -> bool {
    let p = Path::new(path_str);
    if p.is_absolute() {
        return true;
    }
    let bytes = path_str.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Resolve a directory or file path (such as `WorkingDirectory`, `LogDirectory`, or `StateFile`):
/// 1. Expand `%VARIABLES%`.
/// 2. If rooted, use directly.
/// 3. Otherwise resolve relative to `base_dir` (the wrapper executable directory).
pub fn resolve_path(raw: &str, base_dir: &Path) -> PathBuf {
    let expanded = expand_env_vars(raw.trim());
    let normalized = normalize_separators(&expanded);
    if is_rooted_path(&normalized) {
        PathBuf::from(normalized)
    } else {
        base_dir.join(normalized)
    }
}

/// Resolve an executable setting (`ExecutablePath`) following Phase 1 §5.3:
/// 1. Expand `%VARIABLES%`.
/// 2. If the value is rooted, use it.
/// 3. If it contains a directory separator (`/` or `\`), resolve it against `base_dir`.
/// 4. If it is a bare name (e.g. `node`), search `PATH` (and `PATHEXT` on Windows).
pub fn resolve_executable(raw: &str, base_dir: &Path) -> Option<PathBuf> {
    let expanded = expand_env_vars(raw.trim());
    if expanded.is_empty() {
        return None;
    }

    if is_rooted_path(&expanded) {
        let p = PathBuf::from(normalize_separators(&expanded));
        return if p.is_file() { Some(p) } else { None };
    }

    if expanded.contains('/') || expanded.contains('\\') {
        let p = base_dir.join(normalize_separators(&expanded));
        return if p.is_file() { Some(p) } else { None };
    }

    // Bare name: search PATH
    search_in_path(&expanded)
}

fn search_in_path(bare_name: &str) -> Option<PathBuf> {
    let path_var = lookup_env_case_insensitive("PATH")?;
    let path_exts: Vec<String> = if cfg!(windows) {
        lookup_env_case_insensitive("PATHEXT")
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new(), ".exe".to_string(), ".sh".to_string()]
    };

    for dir in env::split_paths(&path_var) {
        let direct = dir.join(bare_name);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in &path_exts {
            if !ext.is_empty() {
                let candidate = dir.join(format!("{bare_name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
                let lower_ext = ext.to_ascii_lowercase();
                if lower_ext != *ext {
                    let candidate_lower = dir.join(format!("{bare_name}{lower_ext}"));
                    if candidate_lower.is_file() {
                        return Some(candidate_lower);
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn expand_env_vars_should_expand_known_variables() {
        // Arrange
        env::set_var("WRAPPER_TEST_DIR", "/custom/root");

        // Act
        let resolved = expand_env_vars("%WRAPPER_TEST_DIR%/bin/node");

        // Assert
        assert_eq!(resolved, "/custom/root/bin/node");
    }

    #[test]
    fn resolve_path_should_resolve_relative_paths_against_base_dir() {
        // Arrange
        let base = Path::new("/opt/service-wrapper/wrapper");

        // Act
        let resolved = resolve_path("..\\apps\\storefront", base);

        // Assert
        assert!(resolved.ends_with("apps/storefront") || resolved.ends_with("apps\\storefront"));
    }

    #[test]
    fn resolve_executable_should_find_relative_and_path_executables() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-custom-tool.exe");
        std::fs::write(&bin_path, b"dummy").unwrap();

        // Act
        let rel = resolve_executable("./my-custom-tool.exe", tmp.path());

        // Assert
        assert_eq!(rel, Some(bin_path));
    }
}
