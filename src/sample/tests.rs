use super::*;

fn proc(pid: i32, ppid: i32, pgid: i32, start: f64) -> Proc {
    Proc {
        pid,
        ppid,
        pgid,
        uid: 501,
        start: format!("{start:.6}"),
    }
}

fn root(id: &str, executor: i32, pgid: i32) -> JobRoot {
    JobRoot {
        id: id.into(),
        executor: Some(executor),
        pgid: Some(pgid),
        since: 100.0,
    }
}

fn sorted(mut v: Vec<i32>) -> Vec<i32> {
    v.sort();
    v
}

#[test]
fn attributes_trees_groups_markers_and_shared_daemons() {
    let procs = vec![
        proc(10, 1, 10, 100.0),  // executor of j1 (session leader)
        proc(11, 10, 11, 101.0), // j1's command, its own group
        proc(12, 11, 11, 102.0), // child
        proc(13, 1, 11, 103.0),  // left the tree, still in the group
        proc(14, 1, 14, 104.0),  // left the tree and the group, marked
        proc(15, 14, 14, 105.0), // its child
        proc(16, 1, 16, 50.0),   // marked, but older than the job: stale
        proc(17, 1, 17, 106.0),  // a shared build daemon
        proc(18, 17, 17, 107.0), // its worker
        proc(19, 11, 11, 108.0), // a daemon still inside the job's tree stays in the job
        proc(20, 1, 20, 1.0),    // unrelated
        proc(30, 1, 30, 100.0),  // executor of j2, whose command has exited
    ];
    let daemon = |p: &Proc| {
        [17, 19]
            .contains(&p.pid)
            .then(|| "GradleDaemon".to_string())
    };
    let marker = |p: &Proc| [14, 16].contains(&p.pid).then(|| "j1".to_string());
    let a = attribute(
        &procs,
        &[root("j1", 10, 11), root("j2", 30, 31)],
        501,
        daemon,
        marker,
    );
    assert_eq!(sorted(a.jobs["j1"].clone()), [11, 12, 13, 14, 15, 19]);
    assert_eq!(a.jobs["j2"], Vec::<i32>::new());
    assert_eq!(sorted(a.shared["GradleDaemon"].clone()), [17, 18]);
}

#[test]
fn a_lost_executor_still_leaves_the_group() {
    // Awaiting takeover: no executor, the group leader carries the job.
    let procs = vec![proc(11, 1, 11, 101.0), proc(12, 11, 11, 102.0)];
    let job = JobRoot {
        id: "j".into(),
        executor: None,
        pgid: Some(11),
        since: 100.0,
    };
    let a = attribute(&procs, &[job], 501, |_| None, |_| None);
    assert_eq!(sorted(a.jobs["j"].clone()), [11, 12]);
}

#[test]
fn other_users_processes_are_never_matched_by_marker_or_name() {
    let mut other = proc(14, 1, 14, 104.0);
    other.uid = 0;
    let a = attribute(
        &[proc(10, 1, 10, 100.0), other],
        &[root("j1", 10, 11)],
        501,
        |p| (p.pid == 14).then(|| "d".into()),
        |p| (p.pid == 14).then(|| "j1".into()),
    );
    assert!(a.jobs["j1"].is_empty() && a.shared.is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn samples_a_running_child() {
    let profiles = Profiles::builtin();
    let mut sampler = Sampler::new(&profiles);
    // A child that holds 64 MB of private memory and uses some CPU.
    let mut child = std::process::Command::new("/usr/bin/perl")
        .args(["-e", "$x = 'a' x (64 << 20); $i++ while $i < 3e7; sleep 5"])
        .spawn()
        .unwrap();
    let job = JobRoot {
        id: "j".into(),
        executor: Some(std::process::id() as i32),
        pgid: None,
        since: 0.0,
    };
    let mut snapshot = sampler.sample(std::slice::from_ref(&job));
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        snapshot = sampler.sample(std::slice::from_ref(&job));
        if snapshot.jobs["j"].peak_private_total > 64 << 20 && snapshot.jobs["j"].cpu_seconds > 0.0
        {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let usage = &snapshot.jobs["j"];
    assert!(usage.processes.contains(&(child.id() as i32)));
    assert!(usage.peak_private_total > 64 << 20, "{usage:?}");
    assert!(usage.cpu_seconds > 0.0, "{usage:?}");
    let system = snapshot.system.unwrap();
    assert!(system.mem_total > 0 && snapshot.boot_id.is_some());
}
