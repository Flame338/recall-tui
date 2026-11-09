use std::{collections::BTreeMap, error::Error, path::Path};

use ignore::WalkBuilder;

use crate::cache::{Scanned, Stamp};

const INDEX_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "rs", "md", "txt", "rst", "py", "go", "java",
    "js", "jsx", "ts", "tsx", "json", "toml", "yaml", "yml", "ini", "sh", "bat", "ps1", "html",
    "css", "sql", "cmake",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;

pub fn walk_project(
    root: &Path,
    mut tick: impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
) -> Result<Option<BTreeMap<String, Scanned>>, Box<dyn Error>> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| kind.is_dir())
                || entry.file_name().to_str().is_some_and(should_descend)
        })
        .build();
    let mut files = BTreeMap::new();
    let mut latest = String::new();
    for entry in walker.filter_map(Result::ok) {
        if !tick(files.len(), &latest)? {
            return Ok(None);
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) || !is_indexable(entry.path()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        latest = relative.to_string_lossy().replace('\\', "/");
        files.insert(
            latest.clone(),
            Scanned {
                path: entry.into_path(),
                stamp: Stamp::of(&metadata),
            },
        );
    }
    Ok(Some(files))
}

fn should_descend(dir_name: &str) -> bool {
    !dir_name.starts_with('.')
        && !matches!(
            dir_name,
            "build" | "target" | "node_modules" | "vendor" | "venv" | "__pycache__"
        )
}

fn is_indexable(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(
        name,
        "Makefile" | "GNUmakefile" | "Dockerfile" | "Justfile" | ".gitignore" | "CMakeLists.txt"
    ) {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            INDEX_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, path::PathBuf, process};

    struct Project(PathBuf);

    impl Project {
        fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!("recall-scan-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn scan(&self) -> Vec<String> {
            walk_project(&self.0, |_, _| Ok(true))
                .unwrap()
                .unwrap()
                .into_keys()
                .collect()
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn gitignored_files_and_folders_are_not_scanned_even_outside_a_git_repo() {
        let project = Project::new("gitignore");
        project.write(".gitignore", "generated/\nsecret.md\n");
        project.write("src/lib.rs", "fn main() {}");
        project.write("src/secret.md", "hidden by pattern at any depth");
        project.write("generated/api.md", "ignored folder");
        project.write("docs/readme.md", "kept");
        assert_eq!(
            project.scan(),
            [".gitignore", "docs/readme.md", "src/lib.rs"]
        );
    }

    #[test]
    fn a_nested_gitignore_only_covers_its_own_folder() {
        let project = Project::new("nested");
        project.write("a/.gitignore", "local.md\n");
        project.write("a/local.md", "ignored here");
        project.write("a/keep.md", "kept");
        project.write("b/local.md", "kept because the rule is in a/");
        assert_eq!(project.scan(), ["a/.gitignore", "a/keep.md", "b/local.md"]);
    }

    #[test]
    fn hidden_folders_build_output_big_files_and_other_extensions_are_skipped() {
        let project = Project::new("rules");
        project.write(".claude/worktrees/x/notes.md", "hidden folder");
        project.write("target/out.md", "build output");
        project.write("node_modules/p/readme.md", "package");
        project.write("image.bin", "not text");
        project.write("big.md", &"x".repeat(600 * 1024));
        project.write("Makefile", "all:");
        project.write("ok.md", "indexed");
        assert_eq!(project.scan(), ["Makefile", "ok.md"]);
    }

    #[test]
    fn a_tick_that_returns_false_cancels_the_walk() {
        let project = Project::new("cancel");
        project.write("a.md", "a");
        project.write("b.md", "b");
        let outcome = walk_project(&project.0, |_, _| Ok(false)).unwrap();
        assert!(outcome.is_none());
    }

    #[test]
    fn hidden_and_build_directories_are_not_descended() {
        for skipped in [
            ".git",
            ".claude",
            ".venv",
            "target",
            "node_modules",
            "build",
        ] {
            assert!(!should_descend(skipped), "{skipped} should be skipped");
        }
        for kept in ["src", "docs", "tests"] {
            assert!(should_descend(kept), "{kept} should be indexed");
        }
    }
}
