//! Table-driven tests over the command examples in the design (6.3, 8.1,
//! 10.7) and the shapes agents commonly send.

use super::*;
use crate::profile::Profiles;

/// A compact rendering of the structure: tools in order, with `seq`, `par`
/// and `bg` showing how they overlap.
fn render(a: &Analysis) -> String {
    fn node(a: &Analysis, n: &Node) -> String {
        match n {
            Node::Cmd { segment } => {
                let s = &a.segments[*segment];
                s.tool.clone().unwrap_or_else(|| "(assign)".into())
            }
            Node::Seq { items } => format!(
                "seq({})",
                items
                    .iter()
                    .map(|i| node(a, i))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Node::Par { items } => format!(
                "par({})",
                items
                    .iter()
                    .map(|i| node(a, i))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Node::Background { item } => format!("bg({})", node(a, item)),
        }
    }
    a.structure
        .as_ref()
        .map_or("unclear".into(), |n| node(a, n))
}

struct Case {
    src: &'static str,
    structure: &'static str,
    light: bool,
    /// Profile of the last segment, or "-" for none.
    profile: &'static str,
}

const fn case(
    src: &'static str,
    structure: &'static str,
    light: bool,
    profile: &'static str,
) -> Case {
    Case {
        src,
        structure,
        light,
        profile,
    }
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    std::fs::create_dir(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"scripts": {"prebuild": "rm -rf dist", "build": "tsc && vite build",
            "pretest": "eslint .", "test": "jest --ci", "dev": "vite"}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("small.txt"), "hello\n").unwrap();
    // A sparse file larger than the light read limit.
    let big = std::fs::File::create(dir.path().join("big.bin")).unwrap();
    big.set_len(64 << 20).unwrap();
    dir
}

const CASES: &[Case] = &[
    // Light commands pass through (6.3); writes and large reads do not.
    case("ls -la", "ls", true, "ls"),
    case(
        "pwd; echo hi > /dev/null 2>&1",
        "seq(pwd,echo)",
        true,
        "shell-builtin",
    ),
    case("echo hi > out.txt", "echo", false, "shell-builtin"),
    case(
        "cat small.txt | head -n 2",
        "par(cat,head)",
        true,
        "read-file",
    ),
    case("cat big.bin", "cat", false, "read-file"),
    case("wc -l < big.bin", "wc", false, "read-file"),
    case("cat *.log", "cat", false, "read-file"),
    case("ls -R /", "ls", false, "ls-recursive"),
    case("rg TODO .", "rg", false, "search"),
    case("mkdir -p build", "mkdir", false, "file-change"),
    case("FOO=1", "(assign)", true, "-"),
    case("cd /tmp && pwd", "seq(cd,pwd)", true, "shell-builtin"),
    case(
        "echo $(git rev-parse HEAD)",
        "seq(git,echo)",
        true,
        "shell-builtin",
    ),
    case(
        "diff <(sort small.txt) <(sort small.txt)",
        "par(diff,sort,sort)",
        true,
        "read-file",
    ),
    case("git status --short", "git", false, "git-read"),
    // Wrappers are removed (8.1 ①).
    case(
        "env RUST_LOG=debug cargo test -p core",
        "cargo",
        false,
        "cargo-test",
    ),
    case(
        "sh -c 'make -j4 && make test'",
        "seq(make,make)",
        false,
        "make",
    ),
    case(
        "bash -lc \"cargo +nightly build --release\"",
        "cargo",
        false,
        "cargo-build",
    ),
    case("npx tsc --noEmit", "tsc", false, "tsc"),
    case(
        "uv run --with rich python -m pytest -q",
        "python",
        false,
        "python-pytest",
    ),
    case("time nice -n 10 make all", "make", false, "make"),
    case("outfilter -q git status", "git", false, "git-read"),
    case("command -v cargo", "command", true, "command-lookup"),
    // Structure (8.1 ②): sequences, pipelines, background jobs.
    case(
        "make -j8 2>&1 | tee build.log",
        "par(make,tee)",
        false,
        "file-change",
    ),
    case(
        "./serve & curl -sf localhost:8080/health; wait -n; pytest",
        "seq(bg(serve),curl,wait,pytest)",
        false,
        "pytest",
    ),
    case(
        "cargo build && cargo test || echo failed",
        "seq(cargo,cargo,echo)",
        false,
        "shell-builtin",
    ),
    case(
        "(cd app && make) > build.log",
        "seq(cd,make)",
        false,
        "make",
    ),
    case(
        "mlx_lm.server --model m &",
        "bg(mlx_lm.server)",
        false,
        "mlx-server",
    ),
    // Package scripts are expanded; scripts and make targets are not (8.1 ③).
    case(
        "cd app && npm run build",
        "seq(cd,seq(rm,seq(tsc,vite)))",
        false,
        "js-bundle",
    ),
    case(
        "cd app && npm test",
        "seq(cd,seq(eslint,jest))",
        false,
        "js-test",
    ),
    case("cd app && pnpm run dev", "seq(cd,vite)", false, "js-bundle"),
    case("npm run build", "npm", false, "-"),
    case("./build.sh", "build.sh", false, "-"),
    case("make release", "make", false, "make"),
    // Commit messages through a here-document.
    case(
        "git commit -m \"$(cat <<'EOF'\nfix: thing\nEOF\n)\"",
        "seq(cat,git)",
        false,
        "git-write",
    ),
    // Unclear structure is handled conservatively.
    case("for f in *.rs; do rustfmt $f; done", "unclear", false, "-"),
    case("$EDITOR notes.md", "$", false, "-"),
    case("eval \"$CMD\"", "eval", false, "-"),
];

#[test]
fn design_examples() {
    let dir = project();
    let profiles = Profiles::builtin();
    let wrappers = vec!["outfilter".to_string()];
    let r = Recognizer::new(&profiles, &wrappers);
    let mut failures = Vec::new();
    for c in CASES {
        let a = r.shell(c.src, dir.path());
        let profile = a
            .segments
            .last()
            .and_then(|s| s.profile.clone())
            .unwrap_or("-".into());
        let got = (render(&a), a.light, profile.as_str());
        if got != (c.structure.to_string(), c.light, c.profile) {
            failures.push(format!(
                "{:?}\n  want {:?} light={} profile={}\n  got  {:?} light={} profile={}",
                c.src, c.structure, c.light, c.profile, got.0, got.1, got.2
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn interactive_commands() {
    let dir = project();
    let profiles = Profiles::builtin();
    let r = Recognizer::new(&profiles, &[]);
    for (src, interactive, light) in [
        ("vim notes.md", true, false),
        ("python3", true, false),
        ("python3 tool.py", false, false),
        ("tail -f app.log", true, true),
        ("git rebase -i HEAD~3", true, false),
        ("git rebase main", false, false),
        ("cargo --version", false, true),
        ("python3 -V", false, true),
        ("cargo build --help", false, false),
    ] {
        let a = r.shell(src, dir.path());
        assert_eq!((a.interactive, a.light), (interactive, light), "{src}");
    }
}

#[test]
fn argv_entry_and_history_keys() {
    let dir = project();
    let profiles = Profiles::builtin();
    let r = Recognizer::new(&profiles, &[]);
    let argv: Vec<String> = ["git", "-C", "repo", "log", "--oneline"]
        .map(String::from)
        .into();
    let a = r.argv(&argv, dir.path());
    let s = &a.segments[0];
    assert_eq!(
        (s.tool.as_deref(), s.subcommand.as_deref()),
        (Some("git"), Some("log"))
    );
    // Arguments are not re-parsed as shell: `>` here is just a word.
    let argv: Vec<String> = ["echo", ">", "x"].map(String::from).into();
    assert!(r.argv(&argv, dir.path()).light);
}

#[test]
fn script_recursion_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts": {"loop": "npm run loop"}}"#,
    )
    .unwrap();
    let profiles = Profiles::builtin();
    let a = Recognizer::new(&profiles, &[]).shell("npm run loop", dir.path());
    assert!(a.structure.is_none() && !a.light);
}
