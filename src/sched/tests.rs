use super::*;

const G: u64 = 1 << 30;

fn cap(used: u64) -> Capacity {
    Capacity {
        memory: 16 * G,
        used,
        cores: 8.0,
        cpu_target: 1.25,
    }
}

fn job(id: &str, age: f64, mem: u64) -> Waiting {
    Waiting {
        id: id.into(),
        age,
        reserved: mem,
        extra: 0,
        cores: 1.0,
        seconds: None,
        locks: Vec::new(),
        deps: Vec::new(),
        now: false,
        ready: true,
        awaited: false,
        session_recency: 0.0,
        urgency: 0,
        pinned: None,
    }
}

fn held(id: &str, reserved: u64, resident: u64) -> Held {
    Held {
        id: id.into(),
        reserved,
        resident,
        extra: 0,
        cores: 1.0,
        locks: Vec::new(),
        ends_in: None,
    }
}

fn run(capacity: &Capacity, held: &[Held], waiting: &[Waiting]) -> Vec<(String, Verdict)> {
    run_with(capacity, held, waiting, 0, Some(10.0))
}

fn run_with(
    capacity: &Capacity,
    held: &[Held],
    waiting: &[Waiting],
    pressure: u8,
    since: Option<f64>,
) -> Vec<(String, Verdict)> {
    let settings = Settings::default();
    decide(&Round {
        capacity,
        held,
        waiting,
        pressure,
        since_admission: since,
        admitting: true,
        settings: &settings,
    })
}

fn admitted(out: &[(String, Verdict)]) -> Vec<&str> {
    out.iter()
        .filter(|(_, v)| *v == Verdict::Admit)
        .map(|(id, _)| id.as_str())
        .collect()
}

fn verdict<'a>(out: &'a [(String, Verdict)], id: &str) -> &'a Verdict {
    &out.iter().find(|(j, _)| j == id).unwrap().1
}

fn path(p: &str) -> Lock {
    Lock::Path(p.into())
}

#[test]
fn headroom_follows_the_formula() {
    // H = C − U − Σ max(0, R − M) − Σ B
    let mut h = held("a", 4 * G, G);
    h.extra = G;
    let mut over = held("b", G, 2 * G); // using more than reserved: nothing pending
    over.extra = 0;
    assert_eq!(
        headroom(&cap(6 * G), &[h, over]),
        (16 - 6 - 3 - 1) as i64 * G as i64
    );
}

#[test]
fn admits_by_memory_in_order() {
    // 16G budget, 8G used: 8G free.
    let out = run(
        &cap(8 * G),
        &[],
        &[
            job("a", 30.0, 3 * G),
            job("b", 20.0, 3 * G),
            job("c", 10.0, 3 * G),
        ],
    );
    assert_eq!(admitted(&out), ["a", "b"]);
    // 2G left is tight: the round closes.
    assert_eq!(
        verdict(&out, "c"),
        &Verdict::Wait {
            reason: Reason::Interval
        }
    );
    let out = run(&cap(14 * G), &[], &[job("a", 30.0, 3 * G)]);
    assert_eq!(
        verdict(&out, "a"),
        &Verdict::Wait {
            reason: Reason::Memory {
                need: 3 * G,
                free: 2 * G as i64
            }
        }
    );
}

#[test]
fn a_tight_round_admits_one() {
    // 4G free: after one 2G admission headroom is below 20% of 16G.
    let out = run(
        &cap(12 * G),
        &[],
        &[job("a", 30.0, 2 * G), job("b", 20.0, G)],
    );
    assert_eq!(admitted(&out), ["a"]);
    assert_eq!(
        verdict(&out, "b"),
        &Verdict::Wait {
            reason: Reason::Interval
        }
    );
}

#[test]
fn waits_for_the_interval_and_stops_under_pressure() {
    let waiting = [job("a", 30.0, G)];
    let out = run_with(&cap(0), &[], &waiting, 0, Some(1.0));
    assert_eq!(
        verdict(&out, "a"),
        &Verdict::Wait {
            reason: Reason::Interval
        }
    );
    let out = run_with(&cap(0), &[], &waiting, 1, None);
    assert_eq!(
        verdict(&out, "a"),
        &Verdict::Wait {
            reason: Reason::Pressure { level: 1 }
        }
    );
}

#[test]
fn cpu_is_soft() {
    let mut big = job("a", 30.0, G);
    big.cores = 20.0;
    // An idle machine takes it anyway.
    assert_eq!(
        admitted(&run(&cap(0), &[], std::slice::from_ref(&big))),
        ["a"]
    );
    // With 8 cores busy (target 10), it waits.
    let busy: Vec<Held> = (0..8).map(|i| held(&format!("h{i}"), 0, 0)).collect();
    let out = run(&cap(0), &busy, &[big]);
    assert!(matches!(
        verdict(&out, "a"),
        Verdict::Wait {
            reason: Reason::Cpu { .. }
        }
    ));
}

#[test]
fn dependencies_block_or_wait() {
    let mut a = job("a", 30.0, G);
    a.deps = vec![("p".into(), Dep::Pending)];
    let mut b = job("b", 20.0, G);
    b.deps = vec![("p".into(), Dep::Succeeded), ("q".into(), Dep::Failed)];
    let mut c = job("c", 10.0, G);
    c.deps = vec![("p".into(), Dep::Succeeded)];
    let out = run(&cap(0), &[], &[a, b, c]);
    assert_eq!(
        verdict(&out, "a"),
        &Verdict::Wait {
            reason: Reason::Dependency { job: "p".into() }
        }
    );
    assert_eq!(verdict(&out, "b"), &Verdict::Blocked { job: "q".into() });
    assert_eq!(admitted(&out), ["c"]);
}

#[test]
fn locks_conflict_with_held_and_newly_admitted_jobs() {
    let mut h = held("h", G, G);
    h.locks = vec![path("/repo/a")];
    let mut a = job("a", 30.0, G);
    a.locks = vec![path("/repo/a/target")];
    let mut b = job("b", 20.0, G);
    b.locks = vec![path("/repo/b")];
    let mut c = job("c", 10.0, G);
    c.locks = vec![path("/repo/b")];
    let out = run(&cap(0), &[h], &[a, b, c]);
    assert_eq!(
        verdict(&out, "a"),
        &Verdict::Wait {
            reason: Reason::Lock {
                resource: "/repo/a".into(),
                holder: "h".into()
            }
        }
    );
    assert_eq!(admitted(&out), ["b"]);
    assert!(
        matches!(verdict(&out, "c"), Verdict::Wait { reason: Reason::Lock { holder, .. } } if holder == "b")
    );
}

#[test]
fn now_skips_admission_but_not_conflicts() {
    let mut a = job("a", 1.0, 20 * G);
    a.now = true;
    let mut b = job("b", 1.0, G);
    b.now = true;
    b.locks = vec![path("/repo")];
    let mut h = held("h", G, G);
    h.locks = vec![path("/repo")];
    let out = run_with(&cap(15 * G), &[h], &[a, b], 2, Some(0.0));
    assert_eq!(
        admitted(&out),
        ["a"],
        "admitted over budget, under pressure, inside the interval"
    );
    assert!(matches!(
        verdict(&out, "b"),
        Verdict::Wait {
            reason: Reason::Lock { .. }
        }
    ));
}

#[test]
fn too_big_for_the_machine() {
    let out = run(&cap(0), &[], &[job("a", 1.0, 17 * G)]);
    assert_eq!(
        verdict(&out, "a"),
        &Verdict::CapacityInsufficient {
            need: 17 * G,
            budget: 16 * G
        }
    );
}

#[test]
fn not_ready_or_recovering() {
    let mut a = job("a", 1.0, G);
    a.ready = false;
    assert_eq!(
        verdict(&run(&cap(0), &[], &[a]), "a"),
        &Verdict::Wait {
            reason: Reason::NoExecutor
        }
    );
    let settings = Settings::default();
    let waiting = [job("b", 1.0, G)];
    let out = decide(&Round {
        capacity: &cap(0),
        held: &[],
        waiting: &waiting,
        pressure: 0,
        since_admission: None,
        admitting: false,
        settings: &settings,
    });
    assert_eq!(
        verdict(&out, "b"),
        &Verdict::Wait {
            reason: Reason::Recovering
        }
    );
}

#[test]
fn ordering_signals() {
    let s = Settings::default();
    let ids = |w: &[Waiting]| {
        order(w, &s)
            .iter()
            .map(|w| w.id.clone())
            .collect::<Vec<_>>()
    };
    let mut awaited = job("awaited", 10.0, G);
    awaited.awaited = true;
    let mut urgent = job("urgent", 10.0, G);
    urgent.urgency = 5; // capped at one step
    let mut recent = job("recent", 10.0, G);
    recent.session_recency = 1.0;
    let old = job("old", 100.0, G);
    let mut low = job("low", 50.0, G);
    low.urgency = -1;
    assert_eq!(
        ids(&[low, recent, old, urgent, awaited]),
        ["awaited", "old", "recent", "urgent", "low"]
    );

    // Starvation: past ten minutes, waiting counts double.
    let mut fresh_awaited = job("fresh", 0.0, G);
    fresh_awaited.awaited = true;
    let starving = job("starving", 650.0, G);
    let mut urgent_newer = job("urgent", 700.0, G);
    urgent_newer.urgency = 1;
    assert_eq!(
        ids(&[fresh_awaited, starving, urgent_newer]),
        ["urgent", "starving", "fresh"]
    );

    // Pinned jobs go first, in pin order.
    let mut p2 = job("p2", 0.0, G);
    p2.pinned = Some(2);
    let mut p1 = job("p1", 0.0, G);
    p1.pinned = Some(1);
    assert_eq!(ids(&[job("x", 999.0, G), p2, p1]), ["p1", "p2", "x"]);
}

#[test]
fn backfills_only_what_does_not_delay_the_head() {
    // 6G free (16 − 10). The head needs 8G; a held job frees 4G in 60s.
    let mut h = held("h", 4 * G, 4 * G);
    h.ends_in = Some(60.0);
    let head = job("head", 50.0, 8 * G);
    let mut quick = job("quick", 40.0, G);
    quick.seconds = Some(30.0);
    let slow = job("slow", 30.0, G); // duration unknown
    let mut long = job("long", 20.0, G);
    long.seconds = Some(120.0);
    let out = run(&cap(10 * G), &[h], &[head, quick, slow, long]);
    assert!(matches!(
        verdict(&out, "head"),
        Verdict::Wait {
            reason: Reason::Memory { .. }
        }
    ));
    assert_eq!(admitted(&out), ["quick"]);
    for id in ["slow", "long"] {
        assert_eq!(
            verdict(&out, id),
            &Verdict::Wait {
                reason: Reason::Behind {
                    head: "head".into()
                }
            }
        );
    }
}

#[test]
fn backfill_never_takes_the_heads_lock() {
    let mut head = job("head", 50.0, 20 * G / 2);
    head.locks = vec![path("/repo")];
    let mut sneaky = job("sneaky", 40.0, 0);
    sneaky.cores = 0.0;
    sneaky.locks = vec![path("/repo/target")];
    let out = run(&cap(8 * G), &[], &[head, sneaky]);
    assert_eq!(
        verdict(&out, "sneaky"),
        &Verdict::Wait {
            reason: Reason::Behind {
                head: "head".into()
            }
        }
    );
}
