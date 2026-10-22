use super::*;

const G: u64 = 1 << 30;

fn cap(used: u64) -> Capacity {
    Capacity {
        memory: 16 * G,
        used,
    }
}

fn job(id: &str, age: f64, mem: u64) -> Waiting {
    Waiting {
        id: id.into(),
        age,
        reserved: mem,
        seconds: None,
        locks: Vec::new(),
        deps: Vec::new(),
        now: false,
        ready: true,
        awaited: false,
    }
}

fn held(id: &str, reserved: u64, resident: u64) -> Held {
    Held {
        id: id.into(),
        reserved,
        resident,
        locks: Vec::new(),
        ends_in: None,
    }
}

fn round(capacity: &Capacity, held: &[Held], waiting: &[Waiting]) -> Round<'static> {
    // Leaked so tests can build rounds inline; tiny and test-only.
    Round {
        capacity: Box::leak(Box::new(capacity.clone())),
        held: Box::leak(held.to_vec().into_boxed_slice()),
        waiting: Box::leak(waiting.to_vec().into_boxed_slice()),
        pressure: false,
        settling: false,
        admitting: true,
    }
}

fn run(capacity: &Capacity, held: &[Held], waiting: &[Waiting]) -> Vec<(String, Verdict)> {
    decide(&round(capacity, held, waiting))
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
    // H = C − U − Σ max(0, R − M)
    let a = held("a", 4 * G, G);
    let over = held("b", G, 2 * G); // using more than reserved: nothing pending
    assert_eq!(
        headroom(&cap(6 * G), &[a, over]),
        (16 - 6 - 3) as i64 * G as i64
    );
}

#[test]
fn admits_by_memory_in_order() {
    // 8G free: two 3G jobs fit; the round then closes as tight.
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
    assert_eq!(
        verdict(&out, "c"),
        &Verdict::Wait {
            reason: Reason::Settling
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
fn many_small_jobs_run_together() {
    // CPU is not an admission constraint: ten 512M jobs all start.
    let waiting: Vec<Waiting> = (0..10)
        .map(|i| job(&format!("j{i}"), 10.0 - i as f64, G / 2))
        .collect();
    assert_eq!(admitted(&run(&cap(2 * G), &[], &waiting)).len(), 10);
}

#[test]
fn pressure_settling_and_recovery_hold_admission() {
    let waiting = [job("a", 30.0, G)];
    let mut r = round(&cap(0), &[], &waiting);
    r.pressure = true;
    assert_eq!(
        verdict(&decide(&r), "a"),
        &Verdict::Wait {
            reason: Reason::Pressure
        }
    );
    r.pressure = false;
    r.settling = true;
    assert_eq!(
        verdict(&decide(&r), "a"),
        &Verdict::Wait {
            reason: Reason::Settling
        }
    );
    r.settling = false;
    r.admitting = false;
    assert_eq!(
        verdict(&decide(&r), "a"),
        &Verdict::Wait {
            reason: Reason::Recovering
        }
    );
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
    let mut r = round(&cap(15 * G), &[h], &[a, b]);
    r.pressure = true;
    r.settling = true;
    let out = decide(&r);
    assert_eq!(
        admitted(&out),
        ["a"],
        "over budget, under pressure, while settling"
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
fn not_ready() {
    let mut a = job("a", 1.0, G);
    a.ready = false;
    assert_eq!(
        verdict(&run(&cap(0), &[], &[a]), "a"),
        &Verdict::Wait {
            reason: Reason::NoExecutor
        }
    );
}

#[test]
fn awaited_jobs_go_first_then_oldest() {
    let mut awaited = job("awaited", 1.0, G);
    awaited.awaited = true;
    let ids: Vec<String> = order(&[job("new", 1.0, G), job("old", 100.0, G), awaited])
        .iter()
        .map(|w| w.id.clone())
        .collect();
    assert_eq!(ids, ["awaited", "old", "new"]);
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
    let mut head = job("head", 50.0, 10 * G);
    head.locks = vec![path("/repo")];
    let mut sneaky = job("sneaky", 40.0, 0);
    sneaky.seconds = Some(1.0);
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
