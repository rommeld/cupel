use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::jobs::child::ManagedChild;
use crate::jobs::{Jobs, MAX_COMPLETED, TAIL_BYTES};

fn shell(script: &str) -> Command {
    let mut cmd = Command::new("/bin/bash");
    cmd.args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

async fn start(jobs: &Jobs, script: &str) -> String {
    jobs.start(shell(script), script, None, &CancellationToken::new())
        .await
        .unwrap()
}

async fn completed(jobs: &Jobs, id: &str) -> (Value, String) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (status, output) = jobs.inspect(id, true).unwrap();
            if !status["exit"].is_null() {
                return (status, output.unwrap());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job did not finish")
}

#[tokio::test]
async fn job_start_interrupted_during_spawn_is_never_published_and_is_reaped() {
    let jobs = Jobs::default();
    let cancel = CancellationToken::new();
    let mut pid = None;
    let (id, interrupted) = jobs
        .spawn("sleep", None, &cancel, || {
            let child = ManagedChild::spawn(shell("exec sleep 30"))?;
            pid = child.group();
            cancel.cancel();
            Ok(child)
        })
        .unwrap();
    assert!(jobs.inspect(&id, false).is_err());
    tokio::time::timeout(Duration::from_secs(5), interrupted.unwrap().cancelled())
        .await
        .unwrap();
    assert!(jobs.require_idle().is_ok());
    assert_eq!(
        rustix::process::test_kill_process(pid.unwrap()),
        Err(rustix::io::Errno::SRCH),
        "cancel during spawn must reap the child even though no handle was returned"
    );
}

#[tokio::test]
async fn noisy_jobs_keep_a_bounded_tail_after_turn_cancellation() {
    let jobs = Jobs::default();
    let cancel = CancellationToken::new();
    let script =
        "printf 'head-marker\\n'; yes x | head -c 1048576; printf 'tail-marker\\n'; exit 37";
    let id = jobs
        .start(shell(script), script, None, &cancel)
        .await
        .unwrap();
    cancel.cancel();
    let (status, output) = completed(&jobs, &id).await;
    assert_eq!(
        status["exit"]["exitCode"], 37,
        "turn abort must not stop the job"
    );
    assert_eq!(status["truncated"], true);
    assert_eq!(status["captureIncomplete"], false);
    assert_eq!(output.len(), TAIL_BYTES);
    assert!(!output.contains("head-marker"));
    assert!(output.ends_with("tail-marker\n"));
}

#[tokio::test]
async fn jobs_reject_excess_live_starts_and_evict_by_completion_order() {
    let jobs = Jobs::default();
    let first = start(&jobs, "exec sleep 30").await;
    let second = start(&jobs, "exec sleep 30").await;
    let cancel = CancellationToken::new();
    let error = jobs
        .start(shell(":"), ":", None, &cancel)
        .await
        .unwrap_err();
    assert!(error.contains("2 managed jobs"), "{error}");
    // The second job completes first, so it is evicted first even though it started later.
    jobs.stop(&second).await.unwrap();
    for _ in 0..MAX_COMPLETED - 1 {
        let id = start(&jobs, "exit 0").await;
        completed(&jobs, &id).await;
    }
    let timed = jobs
        .start(
            shell("exec sleep 30 >/dev/null 2>&1"),
            "sleep",
            Some(Duration::from_millis(100)),
            &cancel,
        )
        .await
        .unwrap();
    let (status, _) = completed(&jobs, &timed).await;
    assert_eq!(
        status["state"], "timedOut",
        "closed pipes must not disable the deadline"
    );
    assert_eq!(status["evictedJobId"], second);
    assert!(jobs.inspect(&second, false).is_err());
    assert_eq!(
        jobs.list().unwrap()["jobs"].as_array().unwrap().len(),
        1 + MAX_COMPLETED
    );
    assert!(jobs.require_idle().unwrap_err().contains(&first));
    jobs.shutdown().await;
}

#[tokio::test]
async fn stop_kills_ordinary_descendants_of_the_shell() {
    let jobs = Jobs::default();
    let id = start(&jobs, "sleep 30 & printf '%s\\n' \"$!\"; wait").await;
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, output) = jobs.inspect(&id, true).unwrap();
            if let Some((pid, _)) = output.unwrap().split_once('\n') {
                break pid.to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    jobs.stop(&id).await.unwrap();
    // The child may briefly be a zombie while init takes ownership. It must
    // not be alive, and the original shell must have been reaped by the job.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let output = Command::new("/bin/ps")
                .args(["-p", &pid, "-o", "stat="])
                .output()
                .await
                .unwrap();
            if output.stdout.is_empty() || output.stdout.starts_with(b"Z") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an ordinary descendant survived jobs stop");
    let (status, _) = jobs.inspect(&id, false).unwrap();
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["exit"]["signal"], 9);
}
