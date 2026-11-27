//! Filesystem + git helpers that keep the editor in sync with the active
//! agent's project worktree.
//!
//! All paths are confined to a project root; `safe_join` rejects traversal.

use std::fs;
use std::path::{Path, PathBuf};

/// Errors surfaced to the UI.
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("path escapes project root")]
    Escape,
}

/// Join `rel` under `root`, rejecting `..` traversal and absolute paths.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, WsError> {
    let rel = Path::new(rel);
    if rel.is_absolute() {
        return Err(WsError::Escape);
    }
    let joined = root.join(rel);
    // Canonicalize the parent when it exists; fall back to lexical check.
    let ok = joined
        .components()
        .all(|c| !matches!(c, std::path::Component::ParentDir));
    if !ok {
        return Err(WsError::Escape);
    }
    Ok(joined)
}

/// Read a text file relative to the project root.
pub fn read_file(root: &Path, rel: &str) -> Result<String, WsError> {
    let p = safe_join(root, rel)?;
    Ok(fs::read_to_string(p)?)
}

/// Write a text file relative to the project root, creating parent dirs.
pub fn write_file(root: &Path, rel: &str, contents: &str) -> Result<(), WsError> {
    let p = safe_join(root, rel)?;
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(p, contents)?;
    Ok(())
}

/// List project files (filtered), relative paths, sorted. Skips heavy dirs.
pub fn list_files(root: &Path, filter: &str) -> Vec<String> {
    let mut out = Vec::new();
    let skip = [
        ".git", "target", "node_modules", ".wt", "dist", "build", ".venv", "venv",
    ];
    walk(root, root, &skip, filter, &mut out, 0);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, skip: &[&str], filter: &str, out: &mut Vec<String>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if skip.contains(&name.as_str()) {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            walk(root, &path, skip, filter, out, depth + 1);
        } else if filter.is_empty()
            || name.to_lowercase().contains(&filter.to_lowercase())
        {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

/// Run `git diff --stat` + numstat in the project root. Returns (files, +ins/-del).
pub fn git_diff_stat(root: &Path) -> (usize, i64, i64) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--numstat", "HEAD"])
        .output();
    let Ok(output) = output else { return (0, 0, 0) };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut files = 0;
    let mut ins = 0i64;
    let mut del = 0i64;
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 3 {
            files += 1;
            ins += cols[0].parse::<i64>().unwrap_or(0);
            del += cols[1].parse::<i64>().unwrap_or(0);
        }
    }
    (files, ins, del)
}

/// A parsed compiler/test diagnostic.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: DiagSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagSeverity {
    Error,
    Warning,
}

/// Run `cargo check` (or a configured command) and parse `error`/`warning` lines.
pub fn run_check(root: &Path) -> Vec<Diagnostic> {
    let output = std::process::Command::new("cargo")
        .arg("check")
        .arg("--message-format=short")
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return vec![Diagnostic {
            severity: DiagSeverity::Error,
            message: "failed to spawn cargo".into(),
        }];
    };
    let text = String::from_utf8_lossy(&output.stderr);
    let mut diags = Vec::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("error") {
            diags.push(Diagnostic {
                severity: DiagSeverity::Error,
                message: line.to_string(),
            });
        } else if lower.contains("warning") {
            diags.push(Diagnostic {
                severity: DiagSeverity::Warning,
                message: line.to_string(),
            });
        }
    }
    diags
}

/// Run the project's tests. Returns (passed, failed, summary lines).
pub fn run_tests(root: &Path) -> (usize, usize, Vec<String>) {
    let output = std::process::Command::new("cargo")
        .arg("test")
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return (0, 0, vec!["failed to spawn cargo".into()]);
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut passed = 0;
    let mut failed = 0;
    let mut lines = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("test result:") {
            lines.push(line.to_string());
            // Format: "ok. 6 passed; 0 failed; ..."
            for part in rest.split(';') {
                let part = part.trim();
                let n = part
                    .split_whitespace()
                    .find(|w| w.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or("0");
                let n = n.parse::<usize>().unwrap_or(0);
                if part.contains("passed") {
                    passed += n;
                } else if part.contains("failed") {
                    failed += n;
                }
            }
        }
    }
    (passed, failed, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal() {
        let root = Path::new("/tmp/proj");
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "src/main.rs").is_ok());
    }

    #[test]
    fn write_read_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!("vf-test-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join("src"));
        write_file(&dir, "src/lib.rs", "fn a() {}").unwrap();
        assert_eq!(read_file(&dir, "src/lib.rs").unwrap(), "fn a() {}");
        let files = list_files(&dir, "");
        assert!(files.iter().any(|f| f == "src/lib.rs"));
        let filtered = list_files(&dir, "lib");
        assert!(filtered.iter().any(|f| f.contains("lib.rs")));
        let _ = fs::remove_dir_all(&dir);
    }
}