//! Command recognition (design 6.3, 8.1): the structure of a command line,
//! each command in it with wrappers removed and scripts expanded, its
//! profile, and whether the whole line is light enough to skip the queue.
//! Recognition only informs prediction and the light check; it never
//! changes what runs.

pub mod shell;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::profile::{Class, Profiles, Writes, positionals};
use shell::{Ast, Redirect, Sep, Simple, Word};

/// How the commands of a line overlap in time (design 8.1 ②). This is an
/// assumption for estimates; resources are held until the job's processes
/// have all exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Node {
    /// One command: an index into [`Analysis::segments`].
    Cmd { segment: usize },
    /// One after another.
    Seq { items: Vec<Node> },
    /// At the same time (pipeline stages, process substitutions).
    Par { items: Vec<Node> },
    /// Started with `&`: assumed to run until the end of the line.
    Background { item: Box<Node> },
}

/// One command after wrappers are removed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    pub argv: Vec<String>,
    /// Program basename and first positional word, for history lookups
    /// (design 8.3).
    pub tool: Option<String>,
    pub subcommand: Option<String>,
    pub profile: Option<String>,
    /// `None` when no profile applies.
    pub class: Option<Class>,
    pub writes: Writes,
    /// Files written by redirections.
    pub redirect_writes: Vec<String>,
    pub interactive: bool,
    pub service: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Analysis {
    /// `None` when the line's structure could not be established.
    pub structure: Option<Node>,
    pub unclear: Option<String>,
    pub segments: Vec<Segment>,
    /// Every command is recognized, light and writes nothing: the line can
    /// run without queueing (design 6.3).
    pub light: bool,
    pub interactive: bool,
}

/// Programs that run the rest of their arguments as a command.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
const MAX_DEPTH: usize = 8;

pub struct Recognizer<'a> {
    profiles: &'a Profiles,
    /// Extra wrapper programs from the configuration (design 8.1 ①).
    wrappers: &'a [String],
}

impl<'a> Recognizer<'a> {
    pub fn new(profiles: &'a Profiles, wrappers: &'a [String]) -> Self {
        Recognizer { profiles, wrappers }
    }

    /// Analyzes shell source run from `cwd`.
    pub fn shell(&self, source: &str, cwd: &Path) -> Analysis {
        let mut run = Run {
            r: self,
            cwd: cwd.to_path_buf(),
            segments: Vec::new(),
            depth: 0,
        };
        let structure = shell::parse(source).and_then(|ast| run.ast(&ast, &[]));
        run.finish(structure)
    }

    /// Analyzes a command given as an argument list.
    pub fn argv(&self, argv: &[String], cwd: &Path) -> Analysis {
        let mut run = Run {
            r: self,
            cwd: cwd.to_path_buf(),
            segments: Vec::new(),
            depth: 0,
        };
        let words: Vec<Word> = argv
            .iter()
            .map(|a| Word {
                text: a.clone(),
                ..Word::default()
            })
            .collect();
        let structure = run.command(words, &[]);
        run.finish(structure)
    }
}

struct Run<'r, 'a> {
    r: &'r Recognizer<'a>,
    cwd: PathBuf,
    segments: Vec<Segment>,
    depth: usize,
}

type Res = Result<Node, shell::Unclear>;

fn seq(mut items: Vec<Node>) -> Node {
    if items.len() == 1 {
        items.pop().expect("one item")
    } else {
        Node::Seq { items }
    }
}

impl Run<'_, '_> {
    fn finish(self, structure: Res) -> Analysis {
        let (structure, unclear) = match structure {
            Ok(n) => (Some(n), None),
            Err(e) => (None, Some(e.0)),
        };
        let light = structure.is_some()
            && !self.segments.is_empty()
            && self.segments.iter().all(|s| {
                s.class == Some(Class::Light)
                    && s.writes == Writes::None
                    && s.redirect_writes.is_empty()
                    && !s.service
            });
        Analysis {
            interactive: self.segments.iter().any(|s| s.interactive),
            structure,
            unclear,
            segments: self.segments,
            light,
        }
    }

    fn ast(&mut self, ast: &Ast, inherited: &[Redirect]) -> Res {
        match ast {
            Ast::Simple(s) => self.simple(s, inherited),
            Ast::Pipeline(stages) => Ok(Node::Par {
                items: stages
                    .iter()
                    .map(|s| self.ast(s, inherited))
                    .collect::<Result<_, _>>()?,
            }),
            Ast::List(items) => {
                let mut nodes = Vec::new();
                for (item, sep) in items {
                    let node = self.ast(item, inherited)?;
                    nodes.push(match sep {
                        Sep::Then => node,
                        Sep::Background => Node::Background {
                            item: Box::new(node),
                        },
                    });
                }
                Ok(seq(nodes))
            }
            Ast::Group(inner, redirects) => {
                let all: Vec<Redirect> = inherited.iter().chain(redirects).cloned().collect();
                let saved = self.cwd.clone();
                let node = self.ast(inner, &all);
                self.cwd = saved;
                node
            }
        }
    }

    fn simple(&mut self, s: &Simple, inherited: &[Redirect]) -> Res {
        let mut before = Vec::new();
        for sub in &s.substitutions {
            before.push(self.ast(sub, &[])?);
        }
        let redirects: Vec<Redirect> = inherited.iter().chain(&s.redirects).cloned().collect();
        let mut main = self.command(s.words.clone(), &redirects)?;
        if !s.processes.is_empty() {
            let mut items = vec![main];
            for p in &s.processes {
                items.push(self.ast(p, &[])?);
            }
            main = Node::Par { items };
        }
        before.push(main);
        Ok(seq(before))
    }

    /// Recognizes one command, removing wrappers and expanding `sh -c` and
    /// package scripts.
    fn command(&mut self, mut words: Vec<Word>, redirects: &[Redirect]) -> Res {
        loop {
            let Some(first) = words.first() else {
                return Ok(self.push(Vec::new(), redirects));
            };
            if first.dynamic {
                return Ok(self.unknown(&words, redirects));
            }
            let prog = basename(&first.text);
            let rest = match prog.as_str() {
                "env" => skip_env(&words[1..]),
                "time" | "nohup" | "exec" => skip_flags(&words[1..], &[]),
                "nice" => skip_flags(&words[1..], &["-n"]),
                "command" if !words[1..].iter().any(|w| w.text == "-v" || w.text == "-V") => {
                    skip_flags(&words[1..], &[])
                }
                "npx" | "pnpx" | "bunx" => skip_flags(&words[1..], &["-p", "--package"]),
                "pnpm" | "yarn"
                    if words
                        .get(1)
                        .is_some_and(|w| w.text == "exec" || w.text == "dlx") =>
                {
                    skip_flags(&words[2..], &[])
                }
                "uv" if words.get(1).is_some_and(|w| w.text == "run") => skip_flags(
                    &words[2..],
                    &[
                        "--with",
                        "--python",
                        "-p",
                        "--project",
                        "--directory",
                        "--package",
                        "--extra",
                        "--group",
                        "--env-file",
                        "--index",
                        "--from",
                        "--with-requirements",
                    ],
                ),
                p if SHELLS.contains(&p) => return self.shell_c(&words, redirects),
                p if self.r.wrappers.iter().any(|w| w == p) => skip_flags(&words[1..], &[]),
                _ => None,
            };
            match rest {
                Some(r) if !r.is_empty() => words = r,
                _ => break,
            }
        }
        let argv: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
        if let Some(node) = self.package_script(&argv, redirects)? {
            return Ok(node);
        }
        if argv[0] == "cd" {
            self.follow_cd(&words);
        }
        let limit = self.r.profiles.find(&argv).and_then(|p| p.max_input);
        let node = self.push(argv, redirects);
        if let (Node::Cmd { segment }, Some(limit)) = (&node, limit) {
            self.check_inputs(*segment, limit, &words, redirects);
        }
        Ok(node)
    }

    /// `sh -c 'source'`: analyzes the source. A shell running a script file
    /// is an unknown command.
    fn shell_c(&mut self, words: &[Word], redirects: &[Redirect]) -> Res {
        let mut i = 1;
        let mut source = None;
        while let Some(w) = words.get(i) {
            if w.text.starts_with('-') && w.text.len() > 1 {
                if w.text.starts_with("--") || !w.text.contains('c') {
                    i += if w.text == "-o" { 2 } else { 1 };
                    continue;
                }
                source = words.get(i + 1);
            }
            break;
        }
        match source {
            Some(src) if !src.dynamic => {
                self.nested(|run| shell::parse(&src.text).and_then(|ast| run.ast(&ast, redirects)))
            }
            _ => Ok(self.unknown(words, redirects)),
        }
    }

    fn nested(&mut self, f: impl FnOnce(&mut Self) -> Res) -> Res {
        if self.depth >= MAX_DEPTH {
            return Err(shell::Unclear("commands nested too deeply".into()));
        }
        self.depth += 1;
        let node = f(self);
        self.depth -= 1;
        node
    }

    /// `npm run x`, `npm test`, `pnpm run x`, `yarn run x`, `bun run x`:
    /// the script from the nearest package.json, with npm's pre and post
    /// scripts (design 8.1 ③).
    fn package_script(
        &mut self,
        argv: &[String],
        redirects: &[Redirect],
    ) -> Result<Option<Node>, shell::Unclear> {
        let tool = basename(&argv[0]);
        if !["npm", "pnpm", "yarn", "bun"].contains(&tool.as_str()) {
            return Ok(None);
        }
        let pos = positionals(&argv[1..], &[]);
        let name = match (tool.as_str(), pos.as_slice()) {
            (_, ["run" | "run-script", name, ..]) => *name,
            ("npm" | "pnpm" | "yarn", ["test" | "t" | "tst", ..]) => "test",
            ("npm" | "pnpm", ["start", ..]) => "start",
            _ => return Ok(None),
        };
        let Some(scripts) = find_scripts(&self.cwd) else {
            return Ok(Some(self.push(argv.to_vec(), redirects)));
        };
        let Some(body) = scripts.get(name).and_then(|v| v.as_str()) else {
            return Ok(Some(self.push(argv.to_vec(), redirects)));
        };
        let mut parts = vec![body.to_string()];
        if tool == "npm" {
            for (key, front) in [(format!("pre{name}"), true), (format!("post{name}"), false)] {
                if let Some(extra) = scripts.get(&key).and_then(|v| v.as_str()) {
                    if front {
                        parts.insert(0, extra.to_string());
                    } else {
                        parts.push(extra.to_string());
                    }
                }
            }
        }
        let mut nodes = Vec::new();
        for part in parts {
            nodes.push(
                self.nested(|run| shell::parse(&part).and_then(|ast| run.ast(&ast, redirects)))?,
            );
        }
        Ok(Some(seq(nodes)))
    }

    fn follow_cd(&mut self, words: &[Word]) {
        match words.get(1) {
            None => {
                if let Some(home) = dirs::home_dir() {
                    self.cwd = home;
                }
            }
            Some(w) if !w.dynamic && w.text != "-" && words.len() == 2 => {
                self.cwd = self.cwd.join(&w.text)
            }
            _ => {}
        }
    }

    fn unknown(&mut self, words: &[Word], redirects: &[Redirect]) -> Node {
        let argv: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
        let node = self.push(argv, redirects);
        if let Node::Cmd { segment } = node {
            let s = &mut self.segments[segment];
            s.profile = None;
            s.class = None;
            s.writes = Writes::Project;
            s.interactive = false;
            s.service = false;
        }
        node
    }

    fn push(&mut self, argv: Vec<String>, redirects: &[Redirect]) -> Node {
        let redirect_writes = redirects
            .iter()
            .filter_map(|r| r.written_file())
            .map(str::to_string)
            .collect();
        let profile = if argv.is_empty() {
            None
        } else {
            self.r.profiles.find(&argv)
        };
        let segment = match (argv.is_empty(), profile) {
            // Only assignments or redirections.
            (true, _) => Segment {
                argv,
                tool: None,
                subcommand: None,
                profile: None,
                class: Some(Class::Light),
                writes: Writes::None,
                redirect_writes,
                interactive: false,
                service: false,
            },
            (false, p) => {
                let value_flags = p.map(|p| p.value_flags.as_slice()).unwrap_or_default();
                let subcommand = positionals(&argv[1..], value_flags)
                    .first()
                    .map(|s| s.to_string());
                Segment {
                    tool: Some(basename(&argv[0])),
                    subcommand,
                    profile: p.map(|p| p.name.clone()),
                    class: p.map(|p| p.class),
                    writes: p.map_or(Writes::Project, |p| p.writes),
                    redirect_writes,
                    interactive: p.is_some_and(|p| {
                        p.interactive || (p.interactive_without_args && argv.len() == 1)
                    }),
                    service: p.is_some_and(|p| p.service),
                    argv,
                }
            }
        };
        self.segments.push(segment);
        Node::Cmd {
            segment: self.segments.len() - 1,
        }
    }

    /// A light command reading a large file, or files it cannot size, is
    /// medium (design 6.3: reading a very large file is not light).
    fn check_inputs(&mut self, segment: usize, limit: u64, words: &[Word], redirects: &[Redirect]) {
        let args = words[1..]
            .iter()
            .filter(|w| !w.text.starts_with('-') && !w.process);
        let inputs = redirects
            .iter()
            .filter_map(|r| r.read_file().map(|f| (f, r.target.dynamic)));
        let too_big = args
            .map(|w| (w.text.as_str(), w.dynamic))
            .chain(inputs)
            .any(|(file, dynamic)| {
                dynamic
                    || std::fs::metadata(self.cwd.join(file))
                        .is_ok_and(|m| m.is_file() && m.len() > limit)
            });
        if too_big && self.segments[segment].class == Some(Class::Light) {
            self.segments[segment].class = Some(Class::Medium);
        }
    }
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// The command after `env`'s options and assignments, if any.
fn skip_env(args: &[Word]) -> Option<Vec<Word>> {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        if matches!(w.text.as_str(), "-u" | "-C" | "-S" | "-P") {
            i += 2;
        } else if w.text.starts_with('-') || (w.text.contains('=') && !w.text.starts_with('=')) {
            i += 1;
        } else {
            break;
        }
    }
    Some(args.get(i..)?.to_vec())
}

/// The command after leading flags; `value_flags` take the next word.
fn skip_flags(args: &[Word], value_flags: &[&str]) -> Option<Vec<Word>> {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        if w.text == "--" {
            i += 1;
            break;
        }
        if !w.text.starts_with('-') {
            break;
        }
        i += if value_flags.contains(&w.text.as_str()) {
            2
        } else {
            1
        };
    }
    Some(args.get(i..)?.to_vec())
}

/// The `scripts` of the nearest package.json at or above `dir`.
fn find_scripts(dir: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let file = dir
        .ancestors()
        .map(|d| d.join("package.json"))
        .find(|p| p.is_file())?;
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    json.get("scripts")?.as_object().cloned()
}

#[cfg(test)]
mod tests;
