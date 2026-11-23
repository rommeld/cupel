use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult};
use cupel_core::types::{ToolResultContent, now_ms};

use crate::jobs::child::ManagedChild;
use crate::jobs::{Jobs, MAX_COMPLETED, TAIL_BYTES, Visibility};
use crate::tools::bash::BashTool;
use crate::tools::jobs::JobsTool;

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

async fn pending_output(jobs: &Jobs) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let output = jobs
                .state
                .lock()
                .unwrap()
                .live
                .iter()
                .find(|job| matches!(job.visibility, Visibility::Pending(_)))
                .map(|job| job.record.capture.output());
            if let Some(output) = output
                && output.ends_with('\n')
            {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("private job did not produce output")
}

fn text(result: &AgentToolResult) -> &str {
    let ToolResultContent::Text(text) = &result.content[0] else {
        panic!("expected text output");
    };
    &text.text
}

#[tokio::test]
async fn bash_yield_returns_inline_exit_and_output_if_already_done() {
    let jobs = Arc::new(Jobs::default());
    let bash = BashTool::new(std::env::temp_dir()).with_jobs(Arc::clone(&jobs));
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        bash.execute(
            "quick",
            json!({"command": "printf 'quick-result\\n'; exit 7", "yield_after_ms": 10_000}),
            CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("a completed job should not wait for the yield deadline")
    .unwrap();
    let details = result.details.as_ref().unwrap();
    assert_eq!(details["state"], "exited");
    assert_eq!(details["exit"]["exitCode"], 7);
    assert!(details.get("id").is_none(), "no handoff occurred");
    assert!(text(&result).ends_with("\n\nquick-result\n"));
    assert_eq!(jobs.list().unwrap(), json!({"jobs": []}));
}

#[tokio::test]
async fn bash_yield_hands_off_once_without_resetting_the_timeout() {
    let cwd = std::env::temp_dir().join(format!(
        "cupel-bash-yield-{}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    let jobs = Arc::new(Jobs::default());
    let bash = BashTool::new(&cwd).with_jobs(Arc::clone(&jobs));
    let cancel = CancellationToken::new();
    let mut call = pin!(bash.execute(
        "slow",
        json!({
            "command": "printf x >> count; printf 'progress\\n'; exec sleep 60",
            "yield_after_ms": 10_000,
            "timeout": 20,
        }),
        cancel.clone(),
        None,
    ));
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    assert_eq!(pending_output(&jobs).await, "progress\n");

    // Advance only after the OS has started the child and its output has been captured.
    tokio::time::pause();
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(10)).await;
    let result = call.await.unwrap();
    let details = result.details.as_ref().unwrap();
    assert_eq!(details["state"], "running");
    let id = details["id"].as_str().unwrap();
    assert!(text(&result).contains("jobs wait"));
    assert!(text(&result).ends_with("\n\nprogress\n"));
    cancel.cancel(); // The Bash call no longer owns the job.

    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    let tool = JobsTool::new(jobs);
    let result = tool
        .execute(
            "final",
            json!({"action": "wait", "id": id, "wait_ms": 2000}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let details = result.details.as_ref().unwrap();
    assert_eq!(
        details["state"], "timedOut",
        "the spawn-time deadline still applies"
    );
    assert_eq!(details["exit"]["signal"], 9);
    assert_eq!(
        std::fs::read_to_string(cwd.join("count")).unwrap(),
        "x",
        "the command must spawn once"
    );
    std::fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn bash_yield_cancellation_or_drop_before_handoff_reaps_the_private_job() {
    let jobs = Arc::new(Jobs::default());
    let bash = BashTool::new(std::env::temp_dir()).with_jobs(Arc::clone(&jobs));
    for cancel_explicitly in [true, false] {
        let cancel = CancellationToken::new();
        let mut call = bash.execute(
            "cancel",
            json!({"command": "printf '%s\\n' \"$$\"; exec sleep 60", "yield_after_ms": 60_000}),
            cancel.clone(),
            None,
        );
        assert!(futures_util::poll!(call.as_mut()).is_pending());
        let raw_pid = pending_output(&jobs).await.trim().parse().unwrap();
        let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
        let finished = jobs.state.lock().unwrap().live[0].finished.clone();
        assert_eq!(
            jobs.list().unwrap(),
            json!({"jobs": []}),
            "the job hasn't yielded yet"
        );
        if cancel_explicitly {
            cancel.cancel();
            let error = tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.to_string(), "Operation aborted");
            assert!(finished.is_cancelled(), "cancel must await cleanup");
        } else {
            drop(call);
            tokio::time::timeout(Duration::from_secs(5), finished.cancelled())
                .await
                .unwrap();
        }
        assert!(jobs.require_idle().is_ok());
        assert_eq!(jobs.list().unwrap(), json!({"jobs": []}));
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
    }
}

#[tokio::test]
async fn bash_yield_and_background_cannot_be_combined() {
    let bash = BashTool::new(std::env::temp_dir()).with_jobs(Arc::new(Jobs::default()));
    let error = bash
        .execute(
            "invalid",
            json!({"command": ":", "background": true, "yield_after_ms": 0}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Cannot combine"));
}

#[tokio::test]
async fn job_start_interrupted_during_spawn_is_never_published_and_is_reaped() {
    let jobs = Jobs::default();
    let cancel = CancellationToken::new();
    let mut pid = None;
    let started = jobs
        .spawn("sleep", None, Visibility::Published, &cancel, || {
            let child = ManagedChild::spawn(shell("exec sleep 30"))?;
            pid = child.group();
            cancel.cancel();
            Ok(child)
        })
        .unwrap();
    assert!(started.interrupted);
    assert!(jobs.inspect(&started.id, false).is_err());
    tokio::time::timeout(Duration::from_secs(5), started.finished.cancelled())
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
async fn wait_expiry_returns_output_without_stopping_the_job() {
    let jobs = Jobs::default();
    let id = start(&jobs, "printf ready; exec sleep 30").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while jobs.inspect(&id, true).unwrap().1.as_deref() != Some("ready") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job did not produce its initial output");

    tokio::time::pause();
    let before = tokio::time::Instant::now();
    let duration = Duration::from_millis(20);
    let cancel = CancellationToken::new();
    let (status, output) =
        tokio::time::timeout(Duration::from_secs(5), jobs.wait(&id, duration, &cancel))
            .await
            .expect("wait ignored its deadline")
            .unwrap();
    assert!(
        before.elapsed() >= duration,
        "wait returned before its deadline"
    );
    assert_eq!(status["state"], "running");
    assert!(status["exit"].is_null());
    assert_eq!(output.as_deref(), Some("ready"));

    tokio::time::resume();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(jobs.wait(&id, Duration::from_secs(30), &cancel), async {
            jobs.stop(&id).await.unwrap();
        })
    })
    .await
    .expect("stop should also finish an active wait");
    let (status, _) = result.unwrap();
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["exit"]["signal"], 9);
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
    let (status, _) = jobs
        .wait(&timed, Duration::from_secs(5), &cancel)
        .await
        .unwrap();
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
