//! `cupel` - entry point: parse args, wire the agent, pick a frontend.
//!
//! Usage:
//!   cupel [--model <id>] [--thinking off|minimal|low|medium|high|xhigh] [--plain]
//!
//! Frontend selection: the ratatui TUI when stdout is a real terminal, the
//! plain line REPL when piped or when `--plain` is given.
//!
//! Model selection: `--model` picks from the built-in catalog; without it,
//! the first provider with credentials in the environment wins
//! (`ANTHROPIC_API_KEY`, then `OPENAI_API_KEY`, then AWS credentials).

use std::io::IsTerminal as _;
use std::sync::Arc;

use cupel_agent::{Agent, AgentOptions, ToolExecutionMode, types::AgentTool};
use cupel_coding_agent::modes::{self, SessionMeta};
use cupel_coding_agent::search::GrepSearch;
use cupel_coding_agent::system_prompt::build_system_prompt;
use cupel_coding_agent::tools::{
    bash::BashTool, edit::EditTool, grep::GrepTool, read::ReadTool, write::WriteTool,
};
use cupel_core::types::{Model, ThinkingLevel};

fn main() -> std::process::ExitCode {
    // Build the runtime explicitly instead of `#[tokio::main]` - same thing,
    // but you can see the moving part.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// What `--resume` asked for: the newest session of this project, or one
/// named by id.
enum ResumeTarget {
    Latest,
    Id(String),
}

struct CliArgs {
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    plain: bool,
    resume: Option<ResumeTarget>,
}

/// Parameterized on the iterator (instead of reading `std::env::args`
/// internally) so tests can drive it without process-global state.
fn parse_args(args: impl Iterator<Item = String>) -> Result<CliArgs, String> {
    let mut parsed = CliArgs {
        model: None,
        thinking: None,
        plain: false,
        resume: None,
    };
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--model" | "-m" => {
                parsed.model = Some(iter.next().ok_or("--model requires a value")?);
            }
            "--thinking" | "-t" => {
                let value = iter.next().ok_or("--thinking requires a value")?;
                parsed.thinking = match value.as_str() {
                    "off" => None,
                    "minimal" => Some(ThinkingLevel::Minimal),
                    "low" => Some(ThinkingLevel::Low),
                    "medium" => Some(ThinkingLevel::Medium),
                    "high" => Some(ThinkingLevel::High),
                    "xhigh" => Some(ThinkingLevel::XHigh),
                    other => return Err(format!("unknown thinking level: {other}")),
                };
            }
            "--plain" => parsed.plain = true,
            // The id is optional: a bare `--resume` (next arg missing or
            // another flag) means "the newest session of this project".
            "--resume" | "-r" => {
                parsed.resume = match iter.peek() {
                    Some(next) if !next.starts_with('-') => {
                        Some(ResumeTarget::Id(iter.next().expect("peeked")))
                    }
                    _ => Some(ResumeTarget::Latest),
                };
            }
            "--help" | "-h" => {
                let mut help = String::from(
                    "usage: cupel [--model <id>] [--thinking off|minimal|low|medium|high|xhigh] [--resume [id]] [--plain]\n\navailable models:\n",
                );
                // Built-ins + models.json layers; deliberately NOT the
                // ollama probe - help must be instant and never touch the
                // network. Discovered models appear in the TUI's /model.
                let home = cupel_coding_agent::resources::config_home();
                let cwd = std::env::current_dir().unwrap_or_default();
                for model in
                    cupel_coding_agent::models::build_catalog_offline(home.as_deref(), &cwd)
                {
                    help.push_str(&format!("  {} ({})\n", model.id, model.provider.as_str()));
                }
                // `print!` PANICS when stdout is a pipe whose reader closed
                // early (`cupel --help | head`). Writing explicitly and
                // ignoring the error is the unsafe-free version of the usual
                // "reset SIGPIPE" fix.
                use std::io::Write as _;
                let _ = std::io::stdout().write_all(help.as_bytes());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(parsed)
}

/// Pick a model + API key from CLI args and the MERGED catalog (built-ins,
/// models.json layers, discovered local models). Credential knowledge
/// lives in `providers.rs`, shared with the TUI's `/provider` command.
fn select_model(args: &CliArgs, catalog: &[Model]) -> Result<(Model, Option<String>), String> {
    use cupel_coding_agent::providers;

    if let Some(wanted) = &args.model {
        let model = catalog
            .iter()
            .find(|m| m.id == *wanted)
            .cloned()
            .ok_or_else(|| format!("unknown model: {wanted} (see --help for the list)"))?;
        let key = providers::env_api_key(model.provider.as_str());
        return Ok((model, key));
    }

    // No --model, pass 1: first provider with CLOUD credentials wins, in
    // catalog order. Bedrock carries no key through StreamOptions - the
    // AWS chain resolves inside the provider. Keyless local models fall
    // through here (their env var is None), so an exported cloud key
    // always beats a merely-running ollama.
    for model in catalog {
        match model.provider.as_str() {
            "amazon-bedrock" if providers::has_aws_credentials() => {
                return Ok((model.clone(), None));
            }
            provider => {
                if let Some(key) = providers::env_api_key(provider) {
                    return Ok((model.clone(), Some(key)));
                }
            }
        }
    }
    // Pass 2: no cloud credentials anywhere - a keyless local model
    // (discovered ollama, models.json entry) is the last resort before
    // giving up.
    if let Some(model) = catalog.iter().find(|m| providers::is_keyless(m)) {
        return Ok((model.clone(), None));
    }
    Err(
        "no credentials found: set ANTHROPIC_API_KEY, OPENAI_API_KEY, FIREWORKS_API_KEY, \
         or AWS credentials, start a local server (ollama / llama-server - see README \
         'Local models'), or start with an explicit `--model <id>` and enter a key in \
         the TUI via `/provider <name> <api-key>`"
            .to_string(),
    )
}

/// Install the tracing subscriber - the ONE place in the whole workspace
/// that consumes trace data (libraries only emit).
///
/// Opt-in via `RUST_LOG`; without it no subscriber exists and every
/// `tracing::` macro in the libraries compiles down to a branch on a static.
/// Examples:
///   RUST_LOG=cupel_core=info,cupel_agent=info   requests, turns, tools, cost
///   RUST_LOG=cupel_core=trace                   + full request bodies
///
/// Writer selection: plain mode logs to stderr (standard, pipe-friendly);
/// the TUI logs to a file because anything written to the terminal would
/// corrupt the ratatui screen. Returns the log-file path in that case.
fn init_tracing(interactive: bool) -> Option<std::path::PathBuf> {
    use tracing_subscriber::fmt::format::FmtSpan;

    // No RUST_LOG, no subscriber, no overhead.
    std::env::var("RUST_LOG").ok()?;
    let filter = tracing_subscriber::EnvFilter::from_default_env();

    // FmtSpan::CLOSE prints a line when each span ends, WITH its measured
    // duration - that's where provider-request and agent-run timing comes
    // from (the events themselves don't carry durations).
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE);

    if interactive {
        let path = std::env::temp_dir().join(format!("cupel-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).ok()?;
        builder
            .with_ansi(false) // no color codes in files
            .with_writer(std::sync::Mutex::new(file))
            .init();
        Some(path)
    } else {
        builder.with_writer(std::io::stderr).init();
        None
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args(std::env::args().skip(1))?;
    let use_plain = args.plain || !std::io::stdout().is_terminal();
    if let Some(log_path) = init_tracing(!use_plain) {
        // Announced BEFORE the TUI takes the screen; visible in scrollback.
        eprintln!("logging to {}", log_path.display());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // NOTE: the project .cupel/ directory is NOT scaffolded here - the
    // frontends create it on the first agent interaction (resources::
    // ensure_project_dot_cupel), so just launching cupel leaves no trace.

    // ---- Resolve the model catalog ONCE ---------------------------------------
    // Built-ins + models.json layers + ollama discovery (bounded network
    // probe, hence async and done here, never in the frontends' sync key
    // handlers). Everything downstream - model selection, /model,
    // /provider, autocomplete - reads THIS list.
    let registry = Arc::new(cupel_core::default_registry());
    let home = cupel_coding_agent::resources::config_home();
    let models = cupel_coding_agent::models::build_catalog(&registry, home.as_deref(), &cwd).await;
    let (model, api_key) = select_model(&args, &models)?;

    // ---- Wire the agent -----------------------------------------------------
    // The grep tool talks to a CodeSearch backend; today that's GrepSearch,
    // in iteration two an index-backed one from cupel-index slots in here.
    let backend = Arc::new(GrepSearch::new(&cwd));
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(ReadTool::new(&cwd)),
        Arc::new(BashTool::new(&cwd)),
        Arc::new(EditTool::new(&cwd)),
        Arc::new(WriteTool::new(&cwd)),
        Arc::new(GrepTool::new(&cwd, backend)),
    ];
    // Project context (AGENTS.md/CLAUDE.md, eager) comes from the cupel
    // home (~/.cupel, override CUPEL_HOME), the project's .cupel/ directory,
    // and the project cwd - later roots are more specific and win.
    let roots = cupel_coding_agent::resources::default_roots(&cwd);
    let context_files = cupel_coding_agent::resources::load_context_files(&roots);
    // `/name`-invocable prompt templates from <root>/prompts/*.md.
    let templates = cupel_coding_agent::commands::load_prompt_templates(&roots);

    // Name + one-line snippet per tool, shown in the system prompt (the full
    // descriptions travel in the tool schemas).
    let system_prompt = build_system_prompt(
        &cwd,
        &[
            ("read", "Read file contents"),
            ("bash", "Execute bash commands (ls, find, cargo, etc.)"),
            (
                "edit",
                "Make precise file edits with exact text replacement, including multiple \
                 disjoint edits in one call",
            ),
            ("write", "Create or overwrite files"),
            (
                "grep",
                "Search file contents for patterns (respects .gitignore)",
            ),
        ],
        &context_files,
    );

    // ---- Session identity: fresh or resumed ---------------------------------
    // Resume keeps the ORIGINAL session id, so the recorder appends to the
    // same transcript file and external consumers see one continuous
    // session. The seeded messages flow into AgentOptions.messages below.
    let (session_id, seeded_messages) = match &args.resume {
        None => (format!("cupel-{}", cupel_core::types::now_ms()), Vec::new()),
        Some(target) => {
            let path = match target {
                ResumeTarget::Id(id) => {
                    cupel_coding_agent::session::sessions_dir(home.as_deref(), &cwd)
                        .map(|dir| dir.join(format!("{id}.jsonl")))
                        .filter(|p| p.exists())
                        .ok_or_else(|| format!("no session named {id} for this project"))?
                }
                ResumeTarget::Latest => {
                    cupel_coding_agent::session::find_latest(home.as_deref(), &cwd)
                        .ok_or("no previous session to resume for this project")?
                }
            };
            let (header, messages) = cupel_coding_agent::session::load_transcript(&path)?;
            (header.session_id, messages)
        }
    };
    // The bash denylist guard rides the agent loop's before_tool_call veto
    // point: built-in rm -rf protection plus user rules from
    // ~/.cupel/bash-deny and <cwd>/.cupel/bash-deny (see guard.rs).
    let guard = cupel_coding_agent::guard::BashGuard::from_config(home.as_deref(), &cwd);
    let recorder =
        cupel_coding_agent::session::SessionRecorder::new(home, &cwd, &session_id, &model.id);

    let mut options = AgentOptions::new(model.clone(), registry);
    options.system_prompt = system_prompt;
    options.tools = tools;
    options.api_key = api_key;
    options.thinking_level = args.thinking;
    options.tool_execution = ToolExecutionMode::Parallel;
    options.session_id = Some(session_id);
    options.messages = seeded_messages;
    options.hooks = Arc::new(guard);
    let agent = Agent::new(options);

    let meta = SessionMeta {
        model_name: model.name.clone(),
        provider: model.provider.as_str().to_string(),
        cwd: cwd.display().to_string(),
        templates,
        models,
    };

    // ---- Pick a frontend ------------------------------------------------------
    // The TUI takes over the whole screen; that only makes sense on a real
    // terminal. Piped output (cupel < script, CI logs) gets plain mode.
    if use_plain {
        modes::plain::run(agent, &meta, recorder).await
    } else {
        modes::interactive::run(agent, meta, recorder)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliArgs, String> {
        parse_args(args.iter().map(ToString::to_string))
    }

    #[test]
    fn resume_flag_with_and_without_an_id() {
        assert!(parse(&[]).unwrap().resume.is_none());
        // Bare --resume = newest session of this project.
        assert!(matches!(
            parse(&["--resume"]).unwrap().resume,
            Some(ResumeTarget::Latest)
        ));
        // With an id.
        match parse(&["--resume", "cupel-123"]).unwrap().resume {
            Some(ResumeTarget::Id(id)) => assert_eq!(id, "cupel-123"),
            other => panic!("expected Id, got {:?}", other.is_some()),
        }
        // Followed by another flag: the flag is NOT eaten as an id.
        let args = parse(&["--resume", "--plain"]).unwrap();
        assert!(matches!(args.resume, Some(ResumeTarget::Latest)));
        assert!(args.plain);
    }

    #[test]
    fn unknown_arguments_still_error() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--model"]).is_err(), "--model needs a value");
    }

    /// A keyless local model (the ollama-discovery shape). Tests use ONLY
    /// keyless catalogs so pass 1 of select_model (which reads real env
    /// vars - process-global, unmockable without unsafe) can never match,
    /// keeping the tests environment-independent.
    fn keyless_model(id: &str) -> Model {
        let mut model = cupel_core::catalog::builtin_models().remove(0);
        model.id = id.to_string();
        model.provider = cupel_core::types::Provider::from("ollama");
        model.compat = Some(serde_json::json!({"requiresApiKey": false}));
        model
    }

    #[test]
    fn select_model_falls_back_to_keyless_local_models() {
        let args = parse(&[]).unwrap();
        let catalog = vec![keyless_model("qwen3:8b"), keyless_model("llama3:8b")];
        // Pass 2: first keyless model wins, with no key.
        let (model, key) = select_model(&args, &catalog).unwrap();
        assert_eq!(model.id, "qwen3:8b");
        assert!(key.is_none());

        // Explicit --model on a keyless entry also carries no key.
        let args = parse(&["--model", "llama3:8b"]).unwrap();
        let (model, key) = select_model(&args, &catalog).unwrap();
        assert_eq!(model.id, "llama3:8b");
        assert!(key.is_none());

        // Empty catalog: the error mentions the local-server escape hatch.
        let args = parse(&[]).unwrap();
        let err = select_model(&args, &[]).unwrap_err();
        assert!(err.contains("ollama"), "{err}");
    }
}
