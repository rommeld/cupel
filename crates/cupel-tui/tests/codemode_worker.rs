#![cfg(feature = "codemode-preview")]

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use cupel_coding_agent::codemode::{Callback, Coordinator, Failure, Request};
    use serde_json::{Value, json};
    use tokio::sync::Notify;
    use tokio::time::{self, Instant};
    use tokio_util::sync::CancellationToken;

    fn coordinator() -> Coordinator {
        Coordinator::new(env!("CARGO_BIN_EXE_cupel").into())
    }

    async fn no_callback(_: Request, _: CancellationToken) -> Result<Value, Failure> {
        panic!("a pure guest reached the host broker");
    }

    async fn pure(coordinator: &Coordinator, source: &str) -> Result<Vec<Value>, Failure> {
        coordinator
            .run(source.into(), &CancellationToken::new(), no_callback)
            .await
    }

    #[tokio::test]
    async fn only_explicit_callbacks_and_emits_leave_the_guest() {
        let coordinator = coordinator();
        let result = coordinator
            .run(
                r#"
files = grep(pattern="Widget", outputMode="files")["files"]
path = files[0]["path"]
page = read(path, limit=2)
print(page["text"])
emit({"path": path, "lines": page["text"].split("\n")[:1]})
"#
                .into(),
                &CancellationToken::new(),
                |request, _| async move {
                    match request.callback {
                        Callback::Grep => {
                            assert_eq!(
                                request.arguments,
                                json!({"pattern":"Widget","outputMode":"files"})
                            );
                            Ok(json!({"files":[{"path":"src/widget.rs"}]}))
                        }
                        Callback::Read => {
                            assert_eq!(
                                request.arguments,
                                json!({"path":"src/widget.rs","limit":2})
                            );
                            Ok(json!({"text":"pub struct Widget;\nPRIVATE_MARKER"}))
                        }
                    }
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            [json!({"path":"src/widget.rs","lines":["pub struct Widget;"]})]
        );
        for source in [
            "emit(files)",
            "bash(command='ls')",
            "from pathlib import Path\nPath('/etc/passwd').read_text()",
            "import time\ntime.sleep(60)",
        ] {
            assert_eq!(
                pure(&coordinator, source).await,
                Err(Failure::Capability),
                "{source}"
            );
        }
        assert_eq!(
            pure(&coordinator, "emit({'x': None})").await.unwrap(),
            [json!({"x":null})]
        );
    }

    #[tokio::test]
    async fn source_and_emit_limits_count_utf8_and_json_escaping() {
        let coordinator = coordinator();
        let mut source = format!("emit(1)\n#{}a", "é".repeat(8187));
        assert_eq!(source.len(), 16_384);
        assert_eq!(pure(&coordinator, &source).await.unwrap(), [json!(1)]);
        source.push('a');
        assert_eq!(pure(&coordinator, &source).await, Err(Failure::Source));

        let at_limit = "emit(0)\nemit('\\n' * 8190 + 'a')";
        assert_eq!(pure(&coordinator, at_limit).await.unwrap().len(), 2);
        let too_large = "emit(0)\nemit('\\n' * 8190 + 'ab')";
        assert_eq!(pure(&coordinator, too_large).await, Err(Failure::EmitBytes));
        assert_eq!(
            pure(
                &coordinator,
                "emit('PRIVATE_MARKER')\nraise ValueError('PRIVATE_MARKER')"
            )
            .await,
            Err(Failure::Script),
        );
    }

    #[tokio::test]
    async fn callback_limits_are_cumulative_and_do_not_dispatch_an_extra_call() {
        let coordinator = coordinator();
        for (count, expected) in [(16, Ok(vec![json!(1)])), (17, Err(Failure::CallbackCount))] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let source = format!("for i in range({count}): read('file')\nemit(1)");
            let result = coordinator
                .run(source, &CancellationToken::new(), move |_, _| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    async { Ok(Value::Null) }
                })
                .await;
            assert_eq!(result, expected);
            assert_eq!(calls.load(Ordering::Relaxed), 16);
        }
        for (size, expected) in [
            (131_070, Ok(vec![json!(1)])),
            (131_071, Err(Failure::CallbackBytes)),
        ] {
            let result = coordinator
                .run(
                    "emit(1)\nread('file')\ngrep('pattern')".into(),
                    &CancellationToken::new(),
                    move |request, _| async move {
                        let size = if request.callback == Callback::Read {
                            131_070
                        } else {
                            size
                        };
                        Ok(Value::String("x".repeat(size)))
                    },
                )
                .await;
            assert_eq!(result, expected);
        }
    }

    #[tokio::test]
    async fn memory_and_time_failures_leave_the_parent_usable() {
        let coordinator = coordinator();
        assert_eq!(
            pure(&coordinator, "emit(len('x' * (15 * 1024 * 1024)))")
                .await
                .unwrap(),
            [json!(15_728_640)]
        );
        let source = "rows = [str(i) + 'x' * 1024 * 1024 for i in range(17)]\nemit(sum(len(row) for row in rows))";
        assert_eq!(pure(&coordinator, source).await, Err(Failure::Memory));
        let start = Instant::now();
        assert_eq!(
            pure(&coordinator, "emit(1)\nwhile True: pass").await,
            Err(Failure::GuestTime)
        );
        assert!(start.elapsed() < Duration::from_millis(1500));
        assert_eq!(pure(&coordinator, "emit(2)").await.unwrap(), [json!(2)]);
    }

    #[tokio::test]
    async fn cancellation_joins_the_active_callback_before_releasing_admission() {
        let coordinator = coordinator();
        let cancel = CancellationToken::new();
        let entered = Arc::new(Notify::new());
        let cancelled = Arc::new(Notify::new());
        let stopped = Arc::new(Notify::new());
        let signal_entered = Arc::clone(&entered);
        let signal_cancelled = Arc::clone(&cancelled);
        let wait_for_stop = Arc::clone(&stopped);
        let active = coordinator.run("emit(1)\nread('file')".into(), &cancel, move |_, token| {
            let entered = Arc::clone(&signal_entered);
            let cancelled = Arc::clone(&signal_cancelled);
            let stopped = Arc::clone(&wait_for_stop);
            async move {
                entered.notify_one();
                token.cancelled().await;
                cancelled.notify_one();
                stopped.notified().await;
                Err(Failure::Cancelled)
            }
        });
        let mut active = std::pin::pin!(active);
        tokio::select! {
            () = entered.notified() => {},
            result = &mut active => panic!("call finished before callback: {result:?}"),
        }
        cancel.cancel();
        cancelled.notified().await;
        let mut later = std::pin::pin!(pure(&coordinator, "emit(2)"));
        assert!(
            time::timeout(Duration::from_millis(50), &mut later)
                .await
                .is_err()
        );
        stopped.notify_one();
        assert_eq!(active.await, Err(Failure::Cancelled));
        assert_eq!(later.await.unwrap(), [json!(2)]);
    }

    #[tokio::test]
    async fn deadline_cancels_the_callback_even_while_the_guest_is_suspended() {
        let coordinator = coordinator();
        let start = Instant::now();
        let result = coordinator
            .run(
                "read('file')".into(),
                &CancellationToken::new(),
                |_, cancel| async move {
                    cancel.cancelled().await;
                    Err(Failure::Cancelled)
                },
            )
            .await;
        assert_eq!(result, Err(Failure::Deadline));
        assert!(start.elapsed() >= Duration::from_secs(5));
        assert_eq!(pure(&coordinator, "emit(2)").await.unwrap(), [json!(2)]);
    }
}
