//! Resource prediction for a command line (design 8.3, user decisions 1a
//! and 1b). Each figure comes from the most specific measured history: the
//! same command in the same project, then the same tool and subcommand in
//! the project, then anywhere, then the profile, then a default. Stages
//! that cannot overlap take the larger peak and add their durations; work
//! that may run at the same time adds its memory and takes the longest
//! duration; an unclear line gets the conservative value.

use serde::{Deserialize, Serialize};

use crate::profile::{Class, Profiles};
use crate::recognize::{Analysis, Node, Segment};

/// What a job is expected to use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    /// Peak private memory (R).
    pub private: u64,
    /// Memory outside the private range, from profiles (B).
    pub extra: u64,
    pub cores: f64,
    /// Run time, when every part of the line has history.
    pub seconds: Option<f64>,
    /// Every command in the line may be frozen (design 11): known, marked
    /// freezable, and not transferring over the network.
    pub freezable: bool,
}

/// `--hint mem=6G,cpu=4`: only used where it is higher than the prediction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hint {
    pub mem: Option<u64>,
    pub cpu: Option<f64>,
}

impl Hint {
    pub fn parse(text: &str) -> Result<Hint, String> {
        let mut hint = Hint::default();
        for part in text.split(',').filter(|p| !p.is_empty()) {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("expected key=value in {part:?}"))?;
            match key.trim() {
                "mem" => hint.mem = Some(crate::profile::parse_size(value)?),
                "cpu" => {
                    hint.cpu = Some(
                        value
                            .trim()
                            .parse()
                            .map_err(|_| format!("bad cpu value {value:?}"))?,
                    );
                }
                other => return Err(format!("unknown hint {other:?}; use mem and cpu")),
            }
        }
        Ok(hint)
    }
}

/// Memory assumed for commands with no measured history and no figure in
/// their profile.
pub struct Defaults {
    pub light: u64,
    pub medium: u64,
    /// For unknown and heavy commands: a fraction of total memory (design
    /// 8.3: 15% initially).
    pub conservative: u64,
    pub cores: f64,
}

impl Defaults {
    pub fn for_machine(mem_total: u64, cores: f64) -> Self {
        Defaults {
            light: 64 << 20,
            medium: 512 << 20,
            conservative: mem_total * 15 / 100,
            cores,
        }
    }
}

/// Measured history for one lookup.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub samples: usize,
    pub mem: Option<u64>,
    pub seconds: Option<f64>,
}

impl Stats {
    /// Fewer than five samples: the largest × 1.3; otherwise the 95th
    /// percentile (design 8.3).
    pub fn from_samples(mem: &[u64], seconds: &[f64]) -> Stats {
        fn pick(mut v: Vec<f64>) -> Option<f64> {
            if v.is_empty() {
                return None;
            }
            v.sort_by(f64::total_cmp);
            Some(if v.len() < 5 {
                v[v.len() - 1] * 1.3
            } else {
                v[(v.len() * 95).div_ceil(100) - 1]
            })
        }
        Stats {
            samples: mem.len().max(seconds.len()),
            mem: pick(mem.iter().map(|&m| m as f64).collect()).map(|m| m as u64),
            seconds: pick(seconds.to_vec()),
        }
    }
}

/// What to look up, from most to least specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// The same command line in the same project.
    Line,
    /// The same tool and subcommand in the same project.
    ProjectKey(&'a str),
    /// The same tool and subcommand anywhere.
    GlobalKey(&'a str),
}

/// The tool and subcommand of a command, which history is kept under.
pub fn key(s: &Segment) -> Option<String> {
    let tool = s.tool.as_ref()?;
    Some(match &s.subcommand {
        Some(sub) => format!("{tool} {sub}"),
        None => tool.clone(),
    })
}

/// The history key of a line made of one command, whose measurements
/// belong to that command alone.
pub fn line_key(analysis: &Analysis) -> Option<String> {
    match (&analysis.structure, analysis.segments.as_slice()) {
        (Some(_), [only]) => key(only),
        _ => None,
    }
}

#[derive(Clone, Copy, Default)]
struct Need {
    mem: u64,
    extra: u64,
    cores: f64,
    seconds: Option<f64>,
}

impl Need {
    /// Running at the same time: resources add up, the longest decides the
    /// time.
    fn add(self, o: Need) -> Need {
        Need {
            mem: self.mem + o.mem,
            extra: self.extra + o.extra,
            cores: self.cores + o.cores,
            seconds: self.seconds.zip(o.seconds).map(|(a, b)| a.max(b)),
        }
    }

    /// The larger of two resource needs; time is handled by the caller.
    fn max(self, o: Need) -> Need {
        Need {
            mem: self.mem.max(o.mem),
            extra: self.extra.max(o.extra),
            cores: self.cores.max(o.cores),
            seconds: self.seconds,
        }
    }
}

fn segment(
    s: &Segment,
    profiles: &Profiles,
    d: &Defaults,
    history: &impl Fn(Lookup) -> Stats,
) -> Need {
    let profile = profiles
        .find(&s.argv)
        .filter(|p| Some(&p.name) == s.profile.as_ref());
    let r = profile.map(|p| &p.resources);
    let measured = key(s)
        .map(|k| {
            let project = history(Lookup::ProjectKey(&k));
            if project.samples > 0 {
                project
            } else {
                history(Lookup::GlobalKey(&k))
            }
        })
        .unwrap_or_default();
    let mem = measured
        .mem
        .or(r.and_then(|r| r.mem))
        .unwrap_or(match s.class {
            Some(Class::Light) => d.light,
            Some(Class::Medium) => d.medium,
            Some(Class::Heavy) | None => d.conservative,
        });
    let cores = match r.and_then(|r| r.cpu) {
        Some(c) if c <= 0.0 => d.cores,
        Some(c) => c,
        None => 1.0,
    };
    Need {
        mem,
        extra: r.and_then(|r| r.extra).unwrap_or(0),
        cores,
        seconds: measured.seconds,
    }
}

fn combine(node: &Node, needs: &[Need]) -> Need {
    match node {
        Node::Cmd { segment } => needs[*segment],
        Node::Par { items } => items
            .iter()
            .map(|i| combine(i, needs))
            .reduce(Need::add)
            .unwrap_or_default(),
        Node::Seq { items } => {
            // A background stage keeps running until the end of the line:
            // its resources add to everything after it, and it ends its own
            // time after it starts. Foreground stages add their times.
            let mut background = Need {
                seconds: Some(0.0),
                ..Need::default()
            };
            let mut peak = Need::default();
            let mut elapsed = Some(0.0f64);
            let mut end = Some(0.0f64);
            for i in items {
                match i {
                    Node::Background { item } => {
                        let n = combine(item, needs);
                        end = end
                            .zip(elapsed.zip(n.seconds))
                            .map(|(e, (at, d))| e.max(at + d));
                        background = background.add(n);
                        peak = peak.max(background);
                    }
                    other => {
                        let n = combine(other, needs);
                        elapsed = elapsed.zip(n.seconds).map(|(a, d)| a + d);
                        end = end.zip(elapsed).map(|(e, a)| e.max(a));
                        peak = peak.max(background.add(n));
                    }
                }
            }
            peak.seconds = end;
            peak
        }
        Node::Background { item } => combine(item, needs),
    }
}

pub fn predict(
    analysis: &Analysis,
    profiles: &Profiles,
    defaults: &Defaults,
    hint: &Hint,
    history: &impl Fn(Lookup) -> Stats,
) -> Prediction {
    let needs: Vec<Need> = analysis
        .segments
        .iter()
        .map(|s| segment(s, profiles, defaults, history))
        .collect();
    let mut need = match &analysis.structure {
        Some(node) => combine(node, &needs),
        None => Need {
            mem: defaults.conservative,
            extra: needs.iter().map(|n| n.extra).sum(),
            cores: defaults.cores,
            seconds: None,
        },
    };
    // The whole line measured before beats any combination of its parts.
    let line = history(Lookup::Line);
    need.mem = line.mem.unwrap_or(need.mem);
    need.seconds = line.seconds.or(need.seconds);
    let freezable = analysis.structure.is_some()
        && analysis.segments.iter().all(|s| {
            profiles.find(&s.argv).is_some_and(|p| {
                Some(&p.name) == s.profile.as_ref() && p.freezable && !p.resources.network
            })
        });
    Prediction {
        private: need.mem.max(hint.mem.unwrap_or(0)),
        extra: need.extra,
        cores: need
            .cores
            .max(hint.cpu.unwrap_or(0.0))
            .min(defaults.cores.max(1.0)),
        seconds: need.seconds,
        freezable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognize::Recognizer;

    const G: u64 = 1 << 30;

    fn no_history(_: Lookup) -> Stats {
        Stats::default()
    }

    fn with_history(src: &str, hint: &str, history: impl Fn(Lookup) -> Stats) -> Prediction {
        let profiles = Profiles::builtin();
        let dir = tempfile::tempdir().unwrap();
        let analysis = Recognizer::new(&profiles, &[]).shell(src, dir.path());
        let defaults = Defaults::for_machine(16 * G, 10.0);
        predict(
            &analysis,
            &profiles,
            &defaults,
            &Hint::parse(hint).unwrap(),
            &history,
        )
    }

    fn predict_line(src: &str, hint: &str) -> Prediction {
        with_history(src, hint, no_history)
    }

    fn stats(mem: Option<u64>, secs: f64) -> Stats {
        Stats {
            samples: 3,
            mem,
            seconds: Some(secs),
        }
    }

    #[test]
    fn combines_by_structure() {
        // cargo-build 3G, cargo-test 4G, make 2G (built-in profiles).
        assert_eq!(
            predict_line("cargo build && cargo test", "").private,
            4 * G,
            "one after another: the larger"
        );
        assert_eq!(
            predict_line("make | cargo build", "").private,
            5 * G,
            "a pipeline: the sum"
        );
        assert_eq!(
            predict_line("make & cargo test; wait", "").private,
            6 * G,
            "background keeps running"
        );
        assert_eq!(
            predict_line("./build.sh", "").private,
            16 * G * 15 / 100,
            "unknown: conservative"
        );
        assert_eq!(
            predict_line("for x in a; do make; done", "").private,
            16 * G * 15 / 100,
            "unclear"
        );
        assert_eq!(predict_line("ls", "").private, 64 << 20);
    }

    #[test]
    fn cores_extra_and_freezing() {
        let p = predict_line("cargo build", "");
        assert_eq!(
            (p.cores, p.freezable),
            (10.0, true),
            "cpu 0 means all cores"
        );
        let p = predict_line("mlx_lm.generate --model m", "");
        assert_eq!(p.extra, 6 * G);
        assert!(
            !predict_line("cargo build && ./deploy.sh", "").freezable,
            "unknown commands are not frozen"
        );
        assert!(!predict_line("git clone url", "").freezable);
    }

    #[test]
    fn hints_only_raise() {
        assert_eq!(predict_line("make", "mem=6G,cpu=4").private, 6 * G);
        assert_eq!(predict_line("make", "mem=1G").private, 2 * G);
        assert!(Hint::parse("gpu=1").is_err());
    }

    #[test]
    fn history_rules() {
        let s = Stats::from_samples(&[100, 200], &[10.0]);
        assert_eq!(
            (s.samples, s.mem, s.seconds),
            (2, Some(260), Some(13.0)),
            "few samples: max x 1.3"
        );
        let mem: Vec<u64> = (1..=20).collect();
        let secs: Vec<f64> = (1..=20).map(f64::from).collect();
        let s = Stats::from_samples(&mem, &secs);
        assert_eq!((s.mem, s.seconds), (Some(19), Some(19.0)), "p95 of 1..=20");
        assert_eq!(Stats::from_samples(&[], &[]), Stats::default());
    }

    #[test]
    fn looks_up_from_specific_to_general() {
        // The same line measured before wins.
        let p = with_history("cargo build && cargo test", "", |l| match l {
            Lookup::Line => stats(Some(G), 50.0),
            _ => stats(Some(9 * G), 99.0),
        });
        assert_eq!((p.private, p.seconds), (G, Some(50.0)));
        // Otherwise per command: the project first, then anywhere.
        let p = with_history("cargo build && cargo test", "", |l| match l {
            Lookup::ProjectKey("cargo build") => stats(Some(5 * G), 30.0),
            Lookup::GlobalKey("cargo build") => stats(Some(9 * G), 99.0),
            Lookup::GlobalKey("cargo test") => stats(Some(2 * G), 20.0),
            _ => Stats::default(),
        });
        assert_eq!(
            (p.private, p.seconds),
            (5 * G, Some(50.0)),
            "one after another: larger peak, summed time"
        );
        // A part without history leaves the time unknown and its memory
        // from the profile (cargo-test: 4G).
        let p = with_history("cargo build && cargo test", "", |l| match l {
            Lookup::GlobalKey("cargo build") => stats(Some(G), 30.0),
            _ => Stats::default(),
        });
        assert_eq!((p.private, p.seconds), (4 * G, None));
        // A hint still only raises a measured figure.
        let p = with_history("make", "mem=8G", |_| stats(Some(G), 1.0));
        assert_eq!(p.private, 8 * G);
    }

    #[test]
    fn durations_follow_structure() {
        let secs = |l: Lookup| match l {
            Lookup::GlobalKey("make") => stats(None, 10.0),
            Lookup::GlobalKey("cargo test") => stats(None, 30.0),
            Lookup::GlobalKey(_) => stats(None, 1.0),
            _ => Stats::default(),
        };
        assert_eq!(
            with_history("make | cargo test", "", secs).seconds,
            Some(30.0),
            "parallel: the longest"
        );
        assert_eq!(
            with_history("make; cargo test", "", secs).seconds,
            Some(40.0),
            "sequential: the sum"
        );
        // Started first in the background, the test ends at 30 s, after
        // the foreground's 10 s of make and 1 s of wait.
        assert_eq!(
            with_history("cargo test & make; wait", "", secs).seconds,
            Some(30.0)
        );
    }

    #[test]
    fn keys() {
        let profiles = Profiles::builtin();
        let dir = tempfile::tempdir().unwrap();
        let r = Recognizer::new(&profiles, &[]);
        assert_eq!(
            line_key(&r.shell("cargo test -p x", dir.path())).as_deref(),
            Some("cargo test")
        );
        assert_eq!(line_key(&r.shell("make && make test", dir.path())), None);
    }
}
