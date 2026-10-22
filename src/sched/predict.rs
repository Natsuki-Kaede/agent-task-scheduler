//! Resource prediction for a command line (design 8.3, user decision 1a):
//! stages that cannot overlap take the larger peak, work that may run at
//! the same time adds up, and an unclear line gets the conservative value.

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
    /// Every command in the line may be frozen (design 11).
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
                    )
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

#[derive(Clone, Copy, Default)]
struct Need {
    mem: u64,
    extra: u64,
    cores: f64,
}

impl Need {
    fn add(self, o: Need) -> Need {
        Need {
            mem: self.mem + o.mem,
            extra: self.extra + o.extra,
            cores: self.cores + o.cores,
        }
    }
    fn max(self, o: Need) -> Need {
        Need {
            mem: self.mem.max(o.mem),
            extra: self.extra.max(o.extra),
            cores: self.cores.max(o.cores),
        }
    }
}

fn segment(s: &Segment, profiles: &Profiles, d: &Defaults) -> Need {
    let profile = profiles
        .find(&s.argv)
        .filter(|p| Some(&p.name) == s.profile.as_ref());
    let r = profile.map(|p| &p.resources);
    let mem = r.and_then(|r| r.mem).unwrap_or(match s.class {
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
    }
}

fn combine(node: &Node, needs: &[Need]) -> Need {
    match node {
        Node::Cmd { segment } => needs[*segment],
        Node::Par { items } => items
            .iter()
            .fold(Need::default(), |acc, i| acc.add(combine(i, needs))),
        // A background item keeps running until the end of the line.
        Node::Seq { items } => {
            let mut background = Need::default();
            let mut peak = Need::default();
            for i in items {
                match i {
                    Node::Background { item } => {
                        background = background.add(combine(item, needs));
                        peak = peak.max(background);
                    }
                    other => peak = peak.max(background.add(combine(other, needs))),
                }
            }
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
) -> Prediction {
    let needs: Vec<Need> = analysis
        .segments
        .iter()
        .map(|s| segment(s, profiles, defaults))
        .collect();
    let need = match &analysis.structure {
        Some(node) => combine(node, &needs),
        None => Need {
            mem: defaults.conservative,
            extra: needs.iter().map(|n| n.extra).sum(),
            cores: defaults.cores,
        },
    };
    let freezable = analysis.structure.is_some()
        && analysis.segments.iter().all(|s| {
            profiles
                .find(&s.argv)
                .is_some_and(|p| Some(&p.name) == s.profile.as_ref() && p.freezable)
        });
    Prediction {
        private: need.mem.max(hint.mem.unwrap_or(0)),
        extra: need.extra,
        cores: need
            .cores
            .max(hint.cpu.unwrap_or(0.0))
            .min(defaults.cores.max(1.0)),
        freezable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognize::Recognizer;

    const G: u64 = 1 << 30;

    fn predict_line(src: &str, hint: &str) -> Prediction {
        let profiles = Profiles::builtin();
        let r = Recognizer::new(&profiles, &[]);
        let dir = tempfile::tempdir().unwrap();
        let defaults = Defaults::for_machine(16 * G, 10.0);
        predict(
            &r.shell(src, dir.path()),
            &profiles,
            &defaults,
            &Hint::parse(hint).unwrap(),
        )
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
}
