//! Background job lifecycle, against real processes (`sh`). Unix-only where
//! process groups are asserted.

use harness_core::{Event, Hook, SessionRef, Tool, World};
use harness_tools_shell::background::{JobScope, JobTable, SpawnRequest, Spawned};
use harness_tools_shell::{JobReaperHook, ShellJobKill, ShellJobStatus, ShellSpawn};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn world() -> World {
    // A timestamp is not unique enough: tests run concurrently, and macOS
    // clocks tick in microseconds — two worlds in the same tick share a dir
    // and their job logs overwrite each other. Count instead.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ws = std::env::temp_dir().join(format!(
        "bgjob-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&ws).unwrap();
    harness_context::default_world(&ws)
}

fn sh(script: &str) -> SpawnRequest {
    SpawnRequest {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        scope: JobScope::Run,
    }
}

#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    // Signal 0 probes: ESRCH ⇒ no process in the group is left.
    unsafe { libc::kill(-(pid as i32), 0) == 0 }
}

/// A killed group can linger briefly as zombies (a grandchild reparented to
/// init counts as "existing" until init reaps it — reliably seen on Linux CI),
/// so death is asserted by polling, never by a single immediate probe.
#[cfg(unix)]
async fn assert_group_dies(pid: u32, what: &str) {
    for _ in 0..40 {
        if !group_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}: group {pid} still alive 2s after kill");
}

#[tokio::test]
async fn quick_command_returns_like_exec_and_leaves_no_job() {
    let table = Arc::new(JobTable::new());
    let mut w = world();
    match table.spawn(&sh("echo hi"), &mut w).await.unwrap() {
        Spawned::Exited { status, output } => {
            assert_eq!(status, 0);
            assert!(output.contains("hi"));
        }
        Spawned::Running { .. } => panic!("echo should exit inside the grace window"),
    }
    assert!(table.list(&w).is_empty());
}

#[tokio::test]
async fn long_command_becomes_a_job_with_cursor_based_output() {
    let table = Arc::new(JobTable::new());
    let mut w = world();
    let (id, pid) = match table
        .spawn(&sh("echo first; sleep 30"), &mut w)
        .await
        .unwrap()
    {
        Spawned::Running { id, pid, preview } => {
            // Early output is visible at spawn time…
            assert!(preview.contains("first"), "preview: {preview:?}");
            (id, pid)
        }
        Spawned::Exited { .. } => panic!("sleep 30 cannot have exited"),
    };

    // …and is NOT replayed by status: the cursor moved past it.
    let st = table.status(id, &w).unwrap();
    assert_eq!(st["state"], "running");
    assert_eq!(st["new_output"], "");

    let killed = table.kill(id, &w).await.unwrap();
    assert_eq!(killed["state"], "killed");
    #[cfg(unix)]
    assert_group_dies(pid, "after kill").await;
    // The entry survives as `exited` so a final look at the log still works.
    assert_eq!(table.status(id, &w).unwrap()["state"], "exited");
}

#[cfg(unix)]
#[tokio::test]
async fn kill_takes_down_grandchildren_via_the_process_group() {
    // `sh` forks a `sleep` grandchild — exactly the `go run` shape. Killing
    // only the direct child would leave the sleep running.
    let table = Arc::new(JobTable::new());
    let mut w = world();
    let Spawned::Running { id, pid, .. } = table
        .spawn(&sh("sleep 30 & echo up; wait"), &mut w)
        .await
        .unwrap()
    else {
        panic!("job should be running")
    };
    assert!(group_alive(pid));
    table.kill(id, &w).await.unwrap();
    assert_group_dies(pid, "the forked sleep must die with the group").await;
}

#[tokio::test]
async fn reaper_kills_run_scope_and_spares_session_scope() {
    let table = Arc::new(JobTable::new());
    let mut w = world();
    let Spawned::Running { pid: run_pid, .. } = table.spawn(&sh("sleep 30"), &mut w).await.unwrap()
    else {
        panic!()
    };
    let mut session_req = sh("sleep 30");
    session_req.scope = JobScope::Session;
    let Spawned::Running {
        id: session_id,
        pid: session_pid,
        ..
    } = table.spawn(&session_req, &mut w).await.unwrap()
    else {
        panic!()
    };

    let reaper = JobReaperHook::new(table.clone());
    assert!(reaper.matches(&Event::SessionEnd));
    reaper.fire(&Event::SessionEnd, &mut w);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let jobs = table.list(&w);
    assert_eq!(jobs.len(), 1, "only the session job survives: {jobs:?}");
    assert_eq!(jobs[0]["id"], session_id);
    #[cfg(unix)]
    {
        assert_group_dies(run_pid, "run-scoped job must be reaped").await;
        assert!(group_alive(session_pid), "session-scoped job must survive");
    }

    // Host shutdown ends the survivor too.
    table.kill_all_owned();
    #[cfg(unix)]
    assert_group_dies(session_pid, "kill_all_owned").await;
    let _ = (run_pid, session_pid);
}

#[tokio::test]
async fn jobs_are_invisible_across_actors() {
    let table = Arc::new(JobTable::new());
    let mut alice = world();
    alice.session = Some(SessionRef {
        id: "s1".into(),
        actor: "alice".into(),
        request: "r1".into(),
    });
    let Spawned::Running { id, .. } = table.spawn(&sh("sleep 30"), &mut alice).await.unwrap()
    else {
        panic!()
    };

    let mut bob = world();
    bob.session = Some(SessionRef {
        id: "s2".into(),
        actor: "bob".into(),
        request: "r2".into(),
    });
    assert!(
        table.status(id, &bob).is_none(),
        "bob must not see alice's job"
    );
    assert!(
        table.kill(id, &bob).await.is_none(),
        "bob must not kill alice's job"
    );
    assert!(table.list(&bob).is_empty());

    assert!(table.status(id, &alice).is_some());
    table.kill(id, &alice).await.unwrap();
}

#[tokio::test]
async fn the_three_tools_round_trip() {
    let table = Arc::new(JobTable::new());
    let spawn = ShellSpawn::new(table.clone());
    let status = ShellJobStatus::new(table.clone());
    let kill = ShellJobKill::new(table.clone());
    let mut w = world();

    let r = spawn
        .invoke(
            json!({"program": "sh", "args": ["-c", "echo up; sleep 30"]}),
            &mut w,
        )
        .await
        .unwrap();
    assert!(r.ok);
    let id = r.content["job_id"].as_u64().expect("job_id");
    assert!(r.content["output_so_far"].as_str().unwrap().contains("up"));

    let r = status.invoke(json!({}), &mut w).await.unwrap();
    assert_eq!(r.content["jobs"].as_array().unwrap().len(), 1);

    let r = kill.invoke(json!({"id": id}), &mut w).await.unwrap();
    assert!(r.ok);
    assert_eq!(r.content["state"], "killed");

    let r = status.invoke(json!({"id": id}), &mut w).await.unwrap();
    assert_eq!(r.content["state"], "exited");
}
