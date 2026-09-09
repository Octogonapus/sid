use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Directory/file names skipped during recursive discovery.
const SKIP_NAMES: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".jj",
    "target",
    "node_modules",
    "__pycache__",
    "dist",
    "build",
    ".tox",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
];

/// Resolve the file list: explicit CLI paths, or all regular files under `.` recursively.
pub fn resolve_files(explicit: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    let mut files = Vec::new();
    collect_files(Path::new("."), &mut files)
        .context("scanning current directory for files")?;
    files.sort();
    prefer_github_first(&mut files);
    Ok(files)
}

fn prefer_github_first(files: &mut [PathBuf]) {
    files.sort_by_key(|p| !is_under_github(p));
}

fn is_under_github(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".github")
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if SKIP_NAMES.contains(&name) {
            continue;
        }

        let path = entry.path();
        let meta = entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        if meta.is_dir() {
            collect_files(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_workspace() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sid-files-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn discovers_github_skips_git_and_target() {
        let root = temp_workspace();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("README.md"), "hi\n").unwrap();
        fs::write(root.join("target/debug/sid"), "bin\n").unwrap();
        fs::write(root.join(".git/config"), "x\n").unwrap();
        fs::write(root.join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        fs::write(root.join(".hidden"), "x\n").unwrap();

        let mut files = Vec::new();
        collect_files(&root, &mut files).unwrap();
        files.sort();
        prefer_github_first(&mut files);

        let names: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        assert!(names.contains(&PathBuf::from("README.md")));
        assert!(names.contains(&PathBuf::from("src/main.rs")));
        assert!(names.contains(&PathBuf::from(".github/workflows/ci.yml")));
        assert!(names.contains(&PathBuf::from(".hidden")));
        assert!(!names.iter().any(|p| p.starts_with("target")));
        assert!(!names.iter().any(|p| p.starts_with(".git")));
        assert_eq!(names[0], PathBuf::from(".github/workflows/ci.yml"));
    }

    #[test]
    fn explicit_list_unchanged() {
        let files = resolve_files(vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]).unwrap();
        assert_eq!(
            files,
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]
        );
    }
}
