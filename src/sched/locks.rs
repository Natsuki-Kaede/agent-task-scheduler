//! Locks (design 10.7): path resources conflict when one path is the other
//! or an ancestor of it, compared component by component on the real path;
//! named resources conflict when equal.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::profile::Writes;
use crate::recognize::Analysis;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "resource", rename_all = "lowercase")]
pub enum Lock {
    /// A normalized absolute path.
    Path(String),
    /// A device, port or other named resource.
    Named(String),
}

impl fmt::Display for Lock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lock::Path(p) | Lock::Named(p) => f.write_str(p),
        }
    }
}

impl Lock {
    pub fn conflicts(&self, other: &Lock) -> bool {
        match (self, other) {
            (Lock::Path(a), Lock::Path(b)) => {
                let (a, b) = (Path::new(a), Path::new(b));
                a.starts_with(b) || b.starts_with(a)
            }
            (Lock::Named(a), Lock::Named(b)) => a == b,
            _ => false,
        }
    }

    pub fn path(path: &Path) -> Lock {
        Lock::Path(normalize(path))
    }

    /// A `--lock` value: a path when it looks like one, otherwise a name.
    pub fn parse(value: &str, cwd: &Path) -> Lock {
        if value.contains('/') || value.starts_with('.') || value.starts_with('~') {
            let expanded = match value.strip_prefix('~') {
                Some(rest) => dirs::home_dir()
                    .unwrap_or_default()
                    .join(rest.trim_start_matches('/')),
                None => cwd.join(value),
            };
            Lock::path(&expanded)
        } else {
            Lock::Named(value.to_string())
        }
    }
}

/// The real path, resolved through the nearest existing ancestor when the
/// path does not exist yet. macOS volumes are case-insensitive by default,
/// so paths compare in lower case there.
pub fn normalize(path: &Path) -> String {
    let mut lexical = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other),
        }
    }
    let mut existing = lexical.as_path();
    let mut rest = Vec::new();
    let real = loop {
        if let Ok(real) = existing.canonicalize() {
            break real;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => break existing.to_path_buf(),
        }
    };
    let full = rest.iter().rev().fold(real, |p, n| p.join(n));
    let text = full.to_string_lossy().into_owned();
    if cfg!(target_os = "macos") {
        text.to_lowercase()
    } else {
        text
    }
}

const PROJECT_MARKERS: &[&str] = &[
    "package.json",
    "go.mod",
    "Cargo.toml",
    "pyproject.toml",
    "build.gradle",
    "build.gradle.kts",
    "Package.swift",
    "pom.xml",
    "CMakeLists.txt",
    "Makefile",
];

/// The project around `dir`: the Git worktree root, else the nearest
/// directory with a project marker file, else `dir` (design 10.7).
pub fn project_root(dir: &Path) -> PathBuf {
    if let Some(git) = dir.ancestors().find(|d| d.join(".git").exists()) {
        return git.to_path_buf();
    }
    dir.ancestors()
        .find(|d| PROJECT_MARKERS.iter().any(|m| d.join(m).is_file()))
        .unwrap_or(dir)
        .to_path_buf()
}

/// The locks a command line needs: each command's write scope from its
/// profile, files written by redirections, the project for unknown commands
/// and unclear lines, and the extra `--lock` values.
pub fn for_line(
    analysis: &Analysis,
    cwd: &Path,
    profiles: &crate::profile::Profiles,
    extra: &[String],
) -> Vec<Lock> {
    let mut locks = Vec::new();
    if analysis.structure.is_none() {
        locks.push(Lock::path(&project_root(cwd)));
    }
    for s in &analysis.segments {
        let profile = profiles
            .find(&s.argv)
            .filter(|p| Some(&p.name) == s.profile.as_ref());
        match s.writes {
            Writes::None => {}
            Writes::Args => {
                for arg in s.argv.iter().skip(1).filter(|a| !a.starts_with('-')) {
                    locks.push(Lock::path(&s.cwd.join(arg)));
                }
            }
            Writes::Paths => {
                let root = project_root(&s.cwd);
                for p in profile
                    .map(|p| p.write_paths.as_slice())
                    .unwrap_or_default()
                {
                    locks.push(Lock::path(&root.join(p)));
                }
            }
            Writes::Project => locks.push(Lock::path(&project_root(&s.cwd))),
        }
        for file in &s.redirect_writes {
            locks.push(Lock::path(&s.cwd.join(file)));
        }
    }
    locks.extend(extra.iter().map(|e| Lock::parse(e, cwd)));
    // Drop duplicates and paths inside another locked path.
    let mut out: Vec<Lock> = Vec::new();
    for l in locks {
        let covered = out.iter().any(|o| match (o, &l) {
            (Lock::Path(a), Lock::Path(b)) => Path::new(b).starts_with(a),
            _ => *o == l,
        });
        if !covered {
            out.retain(|o| !matches!((o, &l), (Lock::Path(a), Lock::Path(b)) if Path::new(a).starts_with(b)));
            out.push(l);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profiles;
    use crate::recognize::Recognizer;

    #[test]
    fn path_conflicts_follow_components() {
        let p = |s: &str| Lock::Path(s.into());
        assert!(p("/repo/app").conflicts(&p("/repo/app/target")));
        assert!(p("/repo/app/target").conflicts(&p("/repo/app")));
        assert!(p("/repo/app").conflicts(&p("/repo/app")));
        assert!(
            !p("/repo/app").conflicts(&p("/repo/app2")),
            "not a bare string prefix"
        );
        assert!(!p("/repo/app").conflicts(&Lock::Named("/repo/app".into())));
        assert!(Lock::Named("device:phone".into()).conflicts(&Lock::Named("device:phone".into())));
    }

    #[test]
    fn normalizes_missing_paths_through_existing_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let real = normalize(dir.path());
        let missing = normalize(&dir.path().join("a/../b/./c"));
        assert_eq!(missing, format!("{real}/b/c"));
        if cfg!(target_os = "macos") {
            assert_eq!(
                normalize(&dir.path().join("Target")),
                format!("{real}/target")
            );
        }
    }

    #[test]
    fn finds_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let app = repo.join("packages/app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(app.join("package.json"), "{}").unwrap();
        assert_eq!(project_root(&app.join("src")), app);
        std::fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(
            project_root(&app.join("src")),
            repo,
            "Git worktree root wins"
        );
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(project_root(&plain), plain);
    }

    #[test]
    fn locks_for_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let profiles = Profiles::builtin();
        let r = Recognizer::new(&profiles, &[]);
        let locks = |src: &str, extra: &[&str]| {
            let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
            for_line(&r.shell(src, &root), &root, &profiles, &extra)
                .into_iter()
                .map(|l| match l {
                    Lock::Path(p) => p.replace(&normalize(&root), "<root>"),
                    Lock::Named(n) => format!("named:{n}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(locks("ls -la", &[]), Vec::<String>::new());
        assert_eq!(locks("cargo build", &[]), ["<root>/target"]);
        assert_eq!(locks("cargo build && cargo fmt", &[]), ["<root>"]);
        assert_eq!(locks("./build.sh", &[]), ["<root>"]);
        assert_eq!(
            locks("cd sub && rm -f a.txt b.txt", &[]),
            ["<root>/sub/a.txt", "<root>/sub/b.txt"]
        );
        assert_eq!(locks("echo hi > out.log", &[]), ["<root>/out.log"]);
        assert_eq!(locks("if true; then make; fi", &[]), ["<root>"]);
        assert_eq!(
            locks("adb install app.apk", &["device:phone"]),
            ["named:device:phone"]
        );
    }
}
