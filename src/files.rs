use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

/// Directory/file names always skipped during recursive discovery.
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
///
/// When `respect_gitignore` is true (default), `.gitignore` / git exclude rules are applied.
pub fn resolve_files(explicit: Vec<PathBuf>, respect_gitignore: bool) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    let mut files = collect_files(Path::new("."), respect_gitignore)
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

fn should_skip_name(name: &str) -> bool {
    SKIP_NAMES.contains(&name)
}

fn collect_files(root: &Path, respect_gitignore: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .ignore(respect_gitignore)
        .parents(respect_gitignore)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| !should_skip_name(name))
                .unwrap_or(true)
        })
        .build();

    for entry in walker {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_file() {
            out.push(entry.into_path());
        }
    }
    Ok(out)
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

        let mut files = collect_files(&root, false).unwrap();
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
    fn skips_gitignored_files_by_default() {
        let root = temp_workspace();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "secret.txt\nbuild-out/\n").unwrap();
        fs::write(root.join("keep.txt"), "ok\n").unwrap();
        fs::write(root.join("secret.txt"), "nope\n").unwrap();
        fs::create_dir_all(root.join("build-out")).unwrap();
        fs::write(root.join("build-out/x.txt"), "nope\n").unwrap();

        let files = collect_files(&root, true).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        assert!(names.contains(&PathBuf::from("keep.txt")));
        assert!(names.contains(&PathBuf::from(".gitignore")));
        assert!(!names.iter().any(|p| p.ends_with("secret.txt")));
        assert!(!names.iter().any(|p| p.starts_with("build-out")));
    }

    #[test]
    fn no_ignore_includes_gitignored_files() {
        let root = temp_workspace();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
        fs::write(root.join("secret.txt"), "nope\n").unwrap();

        let files = collect_files(&root, false).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        assert!(names.contains(&PathBuf::from("secret.txt")));
    }

    #[test]
    fn explicit_list_unchanged() {
        let files =
            resolve_files(vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")], true).unwrap();
        assert_eq!(files, vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
    }
}
