use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult};
use cupel_core::types::{ToolResultContent, now_ms};

use crate::jobs::child::ManagedChild;
use crate::jobs::{
    Capture, Exit, Jobs, Live, MAX_COMPLETED, Phase, Record, TAIL_BYTES, Termination, Visibility,
};
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
                .find(|job| matches!(job.visibility, Visibility::Pending { .. }))
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
async fn bash_yield_inline_drains_inherited_output_like_foreground_bash() {
    let cwd =
        std::env::temp_dir().join(format!("cupel-inline-{}-{}", std::process::id(), now_ms()));
    let jobs = Arc::new(Jobs::default());
    let command = "(sleep .10; touch child-ran; printf 'child output\\n') & printf 'shell exit\\n'";
    for (name, args) in [
        ("foreground", json!({"command": command})),
        (
            "inline",
            json!({"command": command, "yield_after_ms": 2000}),
        ),
    ] {
        let dir = cwd.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let result = BashTool::new(&dir)
            .with_jobs(Arc::clone(&jobs))
            .execute(name, args, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(text(&result), "shell exit\nchild output\n", "{name}");
        assert!(dir.join("child-ran").exists(), "{name} killed the child");
        assert!(result.details.is_none(), "{name} returned job details");
    }
    assert_eq!(jobs.list().unwrap(), json!({"jobs": []}));
    std::fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn bash_yield_inline_failure_preserves_full_log() {
    let jobs = Arc::new(Jobs::default());
    let bash = BashTool::new(std::env::temp_dir()).with_jobs(Arc::clone(&jobs));
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        bash.execute(
            "quick",
            json!({
                "command": "printf 'head-marker\\n'; seq 1 30000; printf 'tail-marker\\n'; exit 7",
                "yield_after_ms": 10_000
            }),
            CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("a completed job should not wait for the yield deadline")
    .unwrap_err()
    .to_string();
    assert!(error.ends_with("Command exited with code 7"));
    assert!(error.contains("tail-marker\n"));
    assert!(!error.contains("head-marker"));
    let (_, spill) = error
        .split_once("Full output: ")
        .expect("missing full log path");
    let (spill, _) = spill
        .split_once("]\n\n")
        .expect("missing end of spill notice");
    let full_log = std::fs::read_to_string(spill).unwrap();
    std::fs::remove_file(spill).unwrap();
    assert!(full_log.starts_with("head-marker\n"));
    assert!(full_log.ends_with("tail-marker\n"));
    assert_eq!(jobs.list().unwrap(), json!({"jobs": []}));
}

#[tokio::test]
async fn bash_yield_inline_timeout_is_a_bash_error_not_a_job_result() {
    let jobs = Arc::new(Jobs::default());
    let bash = BashTool::new(std::env::temp_dir()).with_jobs(Arc::clone(&jobs));
    let error = bash
        .execute(
            "quick-timeout",
            json!({"command": "exec sleep 30", "timeout": 0.1, "yield_after_ms": 10_000}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Command timed out after 0.1 seconds");
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
    let private_output = match &jobs.state.lock().unwrap().live[0].visibility {
        Visibility::Pending { output, .. } => Arc::clone(output),
        _ => panic!("job must remain private until the yield deadline"),
    };

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
    assert!(
        private_output.lock().unwrap().is_none(),
        "foreground capture must not survive handoff"
    );
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
            pid = child.leader();
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
async fn attached_wait_keeps_the_result_after_history_eviction() {
    let jobs = Jobs::default();
    let id = start(&jobs, "printf result; exec sleep 30").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while jobs.inspect(&id, true).unwrap().1.as_deref() != Some("result") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job did not produce its initial output");

    let cancel = CancellationToken::new();
    let mut attached = pin!(jobs.wait(&id, Duration::from_secs(30), &cancel));
    let mut reset = pin!(jobs.wait(&id, Duration::from_secs(30), &cancel));
    assert!(futures_util::poll!(attached.as_mut()).is_pending());
    assert!(futures_util::poll!(reset.as_mut()).is_pending());
    jobs.stop(&id).await.unwrap();
    let expected = jobs.inspect(&id, true).unwrap();
    for _ in 0..MAX_COMPLETED {
        let other_id = start(&jobs, ":").await;
        completed(&jobs, &other_id).await;
    }

    assert!(jobs.wait(&id, Duration::ZERO, &cancel).await.is_err());
    let result = attached.await.unwrap();
    assert_eq!(result, expected);
    let (status, output) = result;
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["exit"]["signal"], 9);
    assert_eq!(output.as_deref(), Some("result"));
    jobs.shutdown().await;
    assert!(
        reset.await.is_err(),
        "session reset invalidates attached waits"
    );
}

#[tokio::test]
async fn stop_and_retained_status_report_incomplete_cleanup() {
    let jobs = Arc::new(Jobs::default());
    let tool = JobsTool::new(Arc::clone(&jobs));
    let stop = CancellationToken::new();
    let id = "Jcleanup";
    jobs.state.lock().unwrap().live.push(Live {
        record: Record {
            id: id.to_string(),
            command: "wait".to_string(),
            capture: Capture::default(),
        },
        stop: stop.clone(),
        finished: CancellationToken::new(),
        completion: watch::Sender::new(None),
        visibility: Visibility::Published,
    });
    let mut stopping = tool.execute(
        "stop",
        json!({"action": "stop", "id": id}),
        CancellationToken::new(),
        None,
    );
    assert!(futures_util::poll!(stopping.as_mut()).is_pending());
    assert!(stop.is_cancelled(), "stop did not request cleanup");
    jobs.state.lock().unwrap().complete(
        id,
        Termination {
            phase: Phase::Stopped,
            exit: Exit {
                exit_code: None,
                signal: Some(9),
            },
            cleanup_error: Some("process scan timed out".to_string()),
        },
    );

    let error = stopping.await.unwrap_err().to_string();
    assert!(
        error.contains("Job Jcleanup cleanup was incomplete: process scan timed out"),
        "{error}"
    );
    assert_eq!(jobs.stop(id).await.unwrap_err(), error);
    let (status, _) = jobs.inspect(id, false).unwrap();
    assert_eq!(status["cleanupError"], "process scan timed out");
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
    wait_for_process_exit(&pid).await;
    let (status, _) = jobs.inspect(&id, false).unwrap();
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["exit"]["signal"], 9);
}

async fn wait_for_process_exit(pid: &str) {
    // An orphan can remain a zombie until its new parent reaps it.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let output = Command::new("/bin/ps")
                .args(["-p", pid, "-o", "stat="])
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
    .expect("a descendant survived job cleanup");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn cleanup_stops_job_control_children_without_stopping_other_jobs() {
    #[derive(Clone, Copy, Debug)]
    enum Cleanup {
        Stop,
        Timeout,
        Shutdown,
        ForegroundTimeout,
    }

    let cwd = std::env::temp_dir().join(format!(
        "cupel-job-control-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let script = r#"
        set -m
        (touch ready; for ((i=0; i<250; i++)); do
            if [[ -e release ]]; then touch witness; exit; fi
            sleep .02
        done) </dev/null >/dev/null 2>&1 &
        printf '%s\n' "$!" > pid
        wait
    "#;
    for action in [
        Cleanup::Stop,
        Cleanup::Timeout,
        Cleanup::Shutdown,
        Cleanup::ForegroundTimeout,
    ] {
        let jobs = Jobs::default();
        let other_jobs = Jobs::default();
        let dir = cwd.join(format!("{action:?}"));
        let other_dir = dir.join("other");
        std::fs::create_dir_all(&other_dir).unwrap();
        let mut command = shell(script);
        command.current_dir(&dir);
        let mut other_command = shell(script);
        other_command.current_dir(&other_dir);
        let other_registry = if matches!(action, Cleanup::Shutdown) {
            &other_jobs
        } else {
            &jobs
        };
        let cancel = CancellationToken::new();
        let (id, foreground) = if matches!(action, Cleanup::ForegroundTimeout) {
            let tool = BashTool::new(&dir);
            let args = json!({"command": script, "timeout": 30});
            let task = tokio::spawn(async move {
                tool.execute("foreground", args, CancellationToken::new(), None)
                    .await
            });
            (String::new(), Some(task))
        } else {
            let job_timeout = matches!(action, Cleanup::Timeout).then_some(Duration::from_secs(30));
            let id = jobs
                .start(command, script, job_timeout, &cancel)
                .await
                .unwrap();
            (id, None)
        };
        let other_id = other_registry
            .start(other_command, script, None, &cancel)
            .await
            .unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(dir.join("pid"))
                    && let Some((pid, _)) = contents.split_once('\n')
                    && dir.join("ready").exists()
                    && other_dir.join("ready").exists()
                {
                    break pid.to_string();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("job control children did not start");

        match action {
            Cleanup::Stop => jobs.stop(&id).await.unwrap(),
            Cleanup::Timeout | Cleanup::ForegroundTimeout => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(30)).await;
                tokio::time::resume();
                if let Some(task) = foreground {
                    let error = task.await.unwrap().unwrap_err().to_string();
                    assert!(error.contains("timed out"), "{error}");
                    assert!(!error.contains("cleanup was incomplete"), "{error}");
                } else {
                    let (status, _) = completed(&jobs, &id).await;
                    assert_eq!(status["state"], "timedOut");
                    assert!(status["cleanupError"].is_null(), "{status}");
                }
            }
            Cleanup::Shutdown => jobs.shutdown().await,
        }
        std::fs::File::create(dir.join("release")).unwrap();
        std::fs::File::create(other_dir.join("release")).unwrap();
        completed(other_registry, &other_id).await;
        assert!(
            other_dir.join("witness").exists(),
            "{action:?} killed another job's child"
        );

        wait_for_process_exit(&pid).await;
        assert!(
            !dir.join("witness").exists(),
            "child wrote after {action:?}"
        );
        assert!(jobs.require_idle().is_ok());
    }
    std::fs::remove_dir_all(cwd).unwrap();
}
