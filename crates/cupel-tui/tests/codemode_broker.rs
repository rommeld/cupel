#![cfg(all(feature = "codemode-preview", unix))]

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cupel_agent::agent_loop::AgentEventStream;
    use cupel_agent::types::BeforeToolCallResult;
    use cupel_agent::{Agent, AgentEvent, AgentHooks, AgentOptions};
    use cupel_coding_agent::bootstrap::{self, Frontend, SessionHooks};
    use cupel_coding_agent::codemode::{Coordinator, FILE_WORKER_ARGUMENT, WORKER_ARGUMENT};
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, StopReason,
        StreamOptions, ToolCall, ToolResultContent, Usage, now_ms,
    };
    use futures_util::StreamExt as _;
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    use rustix::process::Pid;
    use serde_json::{Value, json};
    use tokio::time;

    const WAIT: Duration = Duration::from_secs(4);
    const PRIVATE: &str = "BROKER_PRIVATE_SOURCE_MARKER";
    const FIXTURE: &str = r"
files = grep('Widget', outputMode='files')['files']
page = read(files[0]['path'], limit=2)
emit({'path': page['path'], 'observation': page['observation'], 'line': page['text'].splitlines()[0]})
";

    #[derive(Default)]
    struct ProviderFixture {
        next: Mutex<Option<ToolCall>>,
        requests: Mutex<Vec<Context>>,
    }

    impl Provider for ProviderFixture {
        fn api(&self) -> &str {
            "broker-fixture"
        }

        fn stream(
            &self,
            model: &Model,
            context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageStream {
            self.requests.lock().unwrap().push(context);
            let call = self.next.lock().unwrap().take();
            let stop = if call.is_some() {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
            let message = AssistantMessage {
                content: call.into_iter().map(AssistantContent::ToolCall).collect(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason: stop,
                error_message: None,
                error_kind: None,
                timestamp: now_ms(),
            };
            let (stream, sink) = assistant_message_channel();
            let _ = sink.start();
            let _ = sink.done(stop, message);
            stream
        }
    }

    struct HookFixture {
        session: SessionHooks,
        veto: bool,
        reaped_before_callback: Arc<Mutex<Vec<Pid>>>,
    }

    #[async_trait::async_trait]
    impl AgentHooks for HookFixture {
        async fn before_tool_call(
            &self,
            assistant: &AssistantMessage,
            call: &ToolCall,
        ) -> Option<BeforeToolCallResult> {
            if call.id.contains(":callback:") {
                assert!(assistant.content.iter().any(|item| matches!(item, AssistantContent::ToolCall(outer) if outer.name == "codemode")), "hook must receive the real outer turn");
                assert_reaped(&self.reaped_before_callback.lock().unwrap());
                if self.veto {
                    assert!(
                        !worker_pids().iter().any(|(_, file)| *file),
                        "veto must happen before file work starts"
                    );
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(PRIVATE.into()),
                    });
                }
            }
            self.session.before_tool_call(assistant, call).await
        }
    }

    struct Project {
        root: PathBuf,
        cwd: PathBuf,
        home: PathBuf,
    }

    impl Project {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cupel-broker-{}-{}",
                std::process::id(),
                now_ms()
            ));
            fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let cwd = root.join("project");
            let home = root.join("home");
            fs::create_dir_all(cwd.join(".git")).unwrap();
            fs::create_dir_all(&home).unwrap();
            Self { root, cwd, home }
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.cwd.join(name), text).unwrap();
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.cwd.join(name)).unwrap()
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct Session {
        agent: Agent,
        provider: Arc<ProviderFixture>,
        recorder: SessionRecorder,
        next: usize,
    }

    impl Session {
        async fn new(
            project: &Project,
            coordinator: Coordinator,
            name: &str,
            veto: bool,
            reaped_before_callback: Arc<Mutex<Vec<Pid>>>,
        ) -> Self {
            let provider = Arc::new(ProviderFixture::default());
            let mut registry = Registry::new();
            registry.register(Arc::clone(&provider) as Arc<dyn Provider>);
            let mut model = cupel_core::catalog::builtin_models().remove(0);
            model.api = Api::from("broker-fixture");
            let mut ingredients = bootstrap::load(
                &project.cwd,
                Some(project.home.clone()),
                &registry,
                Frontend::Plain,
            )
            .await;
            let recorder =
                SessionRecorder::new(Some(project.home.clone()), &project.cwd, name, &model.id);
            ingredients
                .bind_session_with_codemode(&recorder, &project.cwd, coordinator)
                .unwrap();
            assert!(
                ingredients.system_prompt().contains("\n- codemode\n"),
                "prompt must use the bound tool list"
            );
            let mut options = AgentOptions::new(model, Arc::new(registry));
            options.api_key = Some("fixture".into());
            options.system_prompt = ingredients.system_prompt();
            options.tools = ingredients.tools;
            options.hooks = Arc::new(HookFixture {
                session: ingredients.hooks,
                veto,
                reaped_before_callback,
            });
            Self {
                agent: Agent::new(options),
                provider,
                recorder,
                next: 0,
            }
        }

        fn start(&mut self, name: &str, arguments: Value) -> AgentEventStream {
            self.next += 1;
            let id = format!("fixture_{}", self.next);
            *self.provider.next.lock().unwrap() = Some(ToolCall {
                name: name.into(),
                id,
                arguments,
                thought_signature: None,
            });
            self.agent.prompt_text("fixture tool call").unwrap()
        }

        async fn finish(&mut self, mut events: AgentEventStream) -> Report {
            let report = time::timeout(WAIT, async {
                let mut report = Report::default();
                while let Some(event) = events.next().await {
                    match event {
                        AgentEvent::MessageEnd { message } => self.recorder.record(&message),
                        AgentEvent::ToolExecutionUpdate {
                            tool_name, partial, ..
                        } if tool_name == "codemode" => {
                            report.trace.push(text(&partial.content));
                        }
                        AgentEvent::ToolExecutionEnd {
                            tool_name,
                            result,
                            is_error,
                            ..
                        } => {
                            report
                                .results
                                .push((tool_name, is_error, text(&result.content)));
                        }
                        _ => {}
                    }
                }
                self.agent.wait_for_idle().await;
                report
            })
            .await
            .unwrap();
            assert_eq!(
                report.results.len(),
                1,
                "only the outer tool may have an end event: {report:?}"
            );
            report
        }

        async fn call(&mut self, name: &str, arguments: Value) -> Report {
            let events = self.start(name, arguments);
            self.finish(events).await
        }
    }

    #[derive(Debug, Default)]
    struct Report {
        results: Vec<(String, bool, String)>,
        trace: Vec<String>,
    }

    impl Report {
        fn text(&self) -> &str {
            &self.results[0].2
        }

        fn emitted(&self) -> Value {
            assert!(!self.results[0].1, "script failed: {self:?}");
            serde_json::from_str(self.text()).unwrap()
        }
    }

    fn text(content: &[ToolResultContent]) -> String {
        content
            .iter()
            .filter_map(|item| match item {
                ToolResultContent::Text(text) => Some(text.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect()
    }

    fn fifo(path: &Path) {
        let status = Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(path)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "could not make fixture FIFO {}",
            path.display()
        );
    }

    async fn wait_for_reader(path: &Path) -> File {
        time::timeout(WAIT, async {
            loop {
                match rustix::fs::open(path, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()) {
                    Ok(fd) => return File::from(fd),
                    Err(Errno::NXIO) => time::sleep(Duration::from_millis(5)).await,
                    Err(error) => panic!("open fixture FIFO {}: {error}", path.display()),
                }
            }
        })
        .await
        .unwrap_or_else(|_error| panic!("no worker opened {}", path.display()))
    }

    fn worker_pids() -> Vec<(Pid, bool)> {
        let output = Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .unwrap();
        assert!(output.status.success(), "could not inspect worker PIDs");
        let lines = String::from_utf8(output.stdout).unwrap();
        lines
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = Pid::from_raw(fields.next()?.parse().ok()?)?;
                let parent = Pid::from_raw(fields.next()?.parse().ok()?)?;
                if parent != rustix::process::getpid() {
                    return None;
                }
                match fields.last()? {
                    FILE_WORKER_ARGUMENT => Some((pid, true)),
                    WORKER_ARGUMENT => Some((pid, false)),
                    _ => None,
                }
            })
            .collect()
    }

    fn assert_reaped(pids: &[Pid]) {
        for pid in pids {
            assert_eq!(
                rustix::process::test_kill_process(*pid),
                Err(Errno::SRCH),
                "worker {pid:?} is still alive or unreaped"
            );
        }
    }

    async fn cancel_and_check_queue(
        first: &mut Session,
        active: AgentEventStream,
        next: &mut Session,
        reaped_before_callback: &Mutex<Vec<Pid>>,
    ) {
        let pids = worker_pids();
        assert_eq!(
            pids.len(),
            2,
            "expected a guest and a blocked file worker: {pids:?}"
        );
        assert_eq!(
            pids.iter().filter(|(_, file)| *file).count(),
            1,
            "one worker must be doing file work"
        );
        let active_pids: Vec<_> = pids.into_iter().map(|(pid, _)| pid).collect();
        *reaped_before_callback.lock().unwrap() = active_pids.clone();
        let mut queued = next.start(
            "codemode",
            json!({"code": "emit(read('a.rs', limit=1)['text'])"}),
        );
        time::timeout(WAIT, async {
            while let Some(event) = queued.next().await {
                if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                    return;
                }
            }
            panic!("queued script did not enter the agent tool lifecycle");
        })
        .await
        .unwrap();
        assert_eq!(
            worker_pids().len(),
            2,
            "a queued script must not start a second guest"
        );
        let request_count = first.provider.requests.lock().unwrap().len();
        first.agent.abort();
        let cancelled = first.finish(active).await;
        assert!(
            cancelled.results[0].1,
            "cancellation must produce an error lifecycle: {cancelled:?}"
        );
        assert!(
            cancelled.text().contains("codemode cancelled"),
            "unexpected cancellation result: {cancelled:?}"
        );
        assert_eq!(
            first.provider.requests.lock().unwrap().len(),
            request_count,
            "cancelled call must not trigger another provider request"
        );
        assert_reaped(&active_pids);
        assert_eq!(
            next.finish(queued).await.emitted(),
            json!(["pub struct Widget;\n"])
        );
        reaped_before_callback.lock().unwrap().clear();
    }

    #[tokio::test]
    async fn broker_keeps_session_observations_and_reaps_real_file_work_before_reuse() {
        let project = Project::new();
        project.write(".gitignore", "ignored.rs\n");
        project.write("ignored.rs", "pub struct Widget;\n");
        project.write("a.rs", "pub struct Widget;\nsecond line\n");
        project.write("private.txt", &format!("selected line\n{PRIVATE}\n"));
        project.write("caller.rs", "use app::Widget;\n");
        project.write("other.txt", "old second\n");
        project.write("AGENTS.md", "[Read the guide](guide.md)\n");
        project.write("guide.md", "cached guide text\n");
        let coordinator = Coordinator::new(env!("CARGO_BIN_EXE_cupel").into());
        let reaped = Arc::new(Mutex::new(Vec::new()));
        let mut first = Session::new(
            &project,
            coordinator.clone(),
            "first",
            false,
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;
        let mut next = Session::new(
            &project,
            coordinator.clone(),
            "queued",
            false,
            Arc::clone(&reaped),
        )
        .await;

        project.write("guide.md", "changed on disk\n");
        let cached = first
            .call(
                "codemode",
                json!({"code":"page = read('guide.md')\nemit([page['text'], page['notices']])"}),
            )
            .await
            .emitted();
        assert_eq!(
            cached[0][0], "cached guide text\n",
            "callbacks must use the bound instruction snapshot"
        );
        assert!(
            cached[0][1][0]
                .as_str()
                .unwrap()
                .contains("Session instruction snapshot"),
            "missing snapshot notice"
        );

        let result = first.call("codemode", json!({"code":FIXTURE})).await;
        let actual = result.emitted();
        assert_eq!(
            actual[0]["path"], "a.rs",
            "definition must rank ahead of calls and ignored files"
        );
        assert_eq!(
            actual[0]["line"], "pub struct Widget;",
            "selected line did not survive the callback"
        );
        assert!(
            result.trace[0].starts_with("grep ok (") && result.trace[1].starts_with("read ok ("),
            "unexpected callback trace: {result:?}"
        );
        let id = actual[0]["observation"].as_str().unwrap();
        let patch = "*** Begin Patch\n*** Update File: other.txt\n@@\n-old second\n+new second\n*** Update File: a.rs\n@@\n-pub struct Widget;\n+pub struct Changed;\n*** End Patch";
        project.write(
            "a.rs",
            "pub struct Widget;\nsecond line\nnew unrelated line\n",
        );
        let stale = first
            .call(
                "apply_patch",
                json!({"input":patch,"expectedObservations":[id]}),
            )
            .await;
        assert!(
            stale.text().contains("Stale source observation"),
            "stale observation unexpectedly accepted: {stale:?}"
        );
        assert_eq!(
            project.read("other.txt"),
            "old second\n",
            "stale observation must reject the patch before either write"
        );
        assert_eq!(
            project.read("a.rs"),
            "pub struct Widget;\nsecond line\nnew unrelated line\n"
        );
        let fresh = first
            .call("codemode", json!({"code":FIXTURE}))
            .await
            .emitted();
        let applied = first
            .call(
                "apply_patch",
                json!({"input":patch,"expectedObservations":[fresh[0]["observation"]]}),
            )
            .await;
        assert!(
            !applied.results[0].1,
            "direct patch rejected a current guest observation: {applied:?}"
        );
        assert_eq!(project.read("other.txt"), "new second\n");
        assert!(project.read("a.rs").starts_with("pub struct Changed;\n"));
        project.write("a.rs", "pub struct Widget;\nsecond line\n");

        let selected = first
            .call(
                "codemode",
                json!({"code":"emit(read('private.txt')['text'].splitlines()[0])"}),
            )
            .await;
        assert_eq!(selected.emitted(), json!(["selected line"]));
        for code in [
            "emit(read('private.txt')['text'])\nraise ValueError('stop')",
            "emit(read('private.txt')['text'])\nemit('x' * 17000)",
        ] {
            let error = first.call("codemode", json!({"code":code})).await;
            assert!(
                error.results[0].1,
                "failed script returned success: {error:?}"
            );
            assert!(
                !format!("{error:?}").contains(PRIVATE),
                "failed emit leaked into the result or trace"
            );
        }
        {
            let requests = first.provider.requests.lock().unwrap();
            for request in requests.iter() {
                for message in &request.messages {
                    if let Message::ToolResult(result) = message {
                        assert!(
                            ["codemode", "apply_patch"].contains(&result.tool_name.as_str()),
                            "child result reached provider: {result:?}"
                        );
                        if result.tool_name == "codemode" {
                            assert!(
                                !text(&result.content).contains(PRIVATE),
                                "callback contents leaked to the provider"
                            );
                        }
                    }
                }
            }
        }
        let saved = fs::read_to_string(first.recorder.transcript().unwrap()).unwrap();
        assert!(
            !saved.contains(PRIVATE),
            "callback contents leaked to the transcript"
        );

        let read_pipe = project.cwd.join("wait.txt");
        fifo(&read_pipe);
        let mut veto = Session::new(
            &project,
            coordinator,
            "veto",
            true,
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;
        let blocked = veto
            .call("codemode", json!({"code":"read('wait.txt')"}))
            .await;
        assert!(
            blocked.text().contains("blocked by tool preflight")
                && !blocked.text().contains(PRIVATE),
            "unexpected veto result: {blocked:?}"
        );
        assert_eq!(
            blocked.trace.len(),
            1,
            "one blocked callback must produce one trace entry"
        );
        let active = first.start("codemode", json!({"code":"read('wait.txt')"}));
        let mut writer = wait_for_reader(&read_pipe).await;
        writer.write_all(b"waiting for EOF\n").unwrap();
        cancel_and_check_queue(&mut first, active, &mut next, &reaped).await;
        drop(writer);

        let search_block = project.cwd.join("z-block/.gitignore");
        fs::create_dir_all(search_block.parent().unwrap()).unwrap();
        fifo(&search_block);
        let active = first.start("codemode", json!({"code":"grep('Widget')"}));
        let mut writer = wait_for_reader(&search_block).await;
        writer.write_all(b"# waiting for EOF\n").unwrap();
        cancel_and_check_queue(&mut first, active, &mut next, &reaped).await;
        drop(writer);

        let active = first.start("codemode", json!({"code":"grep('Widget', context=1)"}));
        let writer = wait_for_reader(&search_block).await;
        let source = project.cwd.join("caller.rs");
        fs::remove_file(&source).unwrap();
        fifo(&source);
        drop(writer);
        let mut writer = wait_for_reader(&source).await;
        writer.write_all(b"use app::Widget;\n").unwrap();
        cancel_and_check_queue(&mut first, active, &mut next, &reaped).await;
        drop(writer);
        assert!(
            worker_pids().is_empty(),
            "no worker may survive the final call"
        );
    }
}
