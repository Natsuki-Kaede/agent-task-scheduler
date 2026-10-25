use super::*;

fn proc(pid: i32, ppid: i32, pgid: i32) -> Proc {
    Proc {
        pid,
        ppid,
        pgid,
        start: format!("{pid}.0"),
    }
}

fn sorted(mut v: Vec<i32>) -> Vec<i32> {
    v.sort();
    v
}

#[test]
fn attributes_trees_and_groups() {
    let procs = vec![
        proc(10, 1, 10),  // executor of j1
        proc(11, 10, 11), // j1's command, its own group
        proc(12, 11, 11), // child
        proc(13, 1, 11),  // left the tree, still in the group
        proc(14, 13, 14), // its child in a new group
        proc(20, 1, 20),  // unrelated
        proc(30, 1, 30),  // executor of j2, whose command has exited
    ];
    let root = |id: &str, executor, pgid| JobRoot {
        id: id.into(),
        executor,
        pgid,
    };
    let a = attribute(
        &procs,
        &[
            root("j1", Some(10), Some(11)),
            root("j2", Some(30), Some(31)),
        ],
    );
    assert_eq!(sorted(a["j1"].clone()), [11, 12, 13, 14]);
    assert_eq!(a["j2"], Vec::<i32>::new());
    // Awaiting takeover: no executor, the group carries the job.
    let a = attribute(&procs, &[root("j", None, Some(11))]);
    assert_eq!(sorted(a["j"].clone()), [11, 12, 13, 14]);
}

#[test]
fn samples_a_running_child() {
    let mut sampler = Sampler::default();
    // A child that holds 64 MB of private memory.
    let mut child = std::process::Command::new("perl")
        .args(["-e", "$x = 'a' x (64 << 20); sleep 5"])
        .spawn()
        .unwrap();
    let job = JobRoot {
        id: "j".into(),
        executor: Some(std::process::id() as i32),
        pgid: None,
    };
    let mut snapshot = sampler.sample(std::slice::from_ref(&job));
    for _ in 0..20 {
        if snapshot.jobs["j"].peak_private_total > 64 << 20 {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        snapshot = sampler.sample(std::slice::from_ref(&job));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let usage = &snapshot.jobs["j"];
    assert!(usage.processes.contains(&(child.id() as i32)));
    assert!(usage.peak_private_total > 64 << 20, "{usage:?}");
    assert!(snapshot.system.unwrap().mem_total > 0);
}
