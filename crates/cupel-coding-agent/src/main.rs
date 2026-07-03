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

struct CliArgs {
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    plain: bool,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = CliArgs {
        model: None,
        thinking: None,
        plain: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--model" | "-m" => {
                args.model = Some(iter.next().ok_or("--model requires a value")?);
            }
            "--thinking" | "-t" => {
                let value = iter.next().ok_or("--thinking requires a value")?;
                args.thinking = match value.as_str() {
                    "off" => None,
                    "minimal" => Some(ThinkingLevel::Minimal),
                    "low" => Some(ThinkingLevel::Low),
                    "medium" => Some(ThinkingLevel::Medium),
                    "high" => Some(ThinkingLevel::High),
                    "xhigh" => Some(ThinkingLevel::XHigh),
                    other => return Err(format!("unknown thinking level: {other}")),
                };
            }
            "--plain" => args.plain = true,
            "--help" | "-h" => {
                let mut help = String::from(
                    "usage: cupel [--model <id>] [--thinking off|minimal|low|medium|high|xhigh] [--plain]\n\nbuilt-in models:\n",
                );
                for model in cupel_core::catalog::builtin_models() {
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
    Ok(args)
}

/// Pick a model + API key from CLI args and environment.
fn select_model(args: &CliArgs) -> Result<(Model, Option<String>), String> {
    let catalog = cupel_core::catalog::builtin_models();

    let api_key_for = |provider: &str| -> Option<String> {
        match provider {
            "anthropic" => std::env::var("ANTHROPIC_API_KEY").ok(),
            "openai" => std::env::var("OPENAI_API_KEY").ok(),
            "fireworks" => std::env::var("FIREWORKS_API_KEY").ok(),
            // Bedrock auth runs through the AWS credential chain inside the
            // provider; no key travels through StreamOptions.
            _ => None,
        }
    };

    if let Some(wanted) = &args.model {
        let model = catalog
            .into_iter()
            .find(|m| m.id == *wanted)
            .ok_or_else(|| format!("unknown model: {wanted} (see --help for the list)"))?;
        let key = api_key_for(model.provider.as_str());
        return Ok((model, key));
    }

    // No --model: first provider with credentials wins, in catalog order
    // (Anthropic, OpenAI, Bedrock, then Fireworks).
    let has_aws =
        std::env::var("AWS_ACCESS_KEY_ID").is_ok() || std::env::var("AWS_PROFILE").is_ok();
    for model in catalog {
        match model.provider.as_str() {
            "anthropic" | "openai" | "fireworks" => {
                if let Some(key) = api_key_for(model.provider.as_str()) {
                    return Ok((model, Some(key)));
                }
            }
            "amazon-bedrock" if has_aws => return Ok((model, None)),
            _ => {}
        }
    }
    Err(
        "no credentials found: set ANTHROPIC_API_KEY, OPENAI_API_KEY, FIREWORKS_API_KEY, \
         or AWS credentials"
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
    let args = parse_args()?;
    let use_plain = args.plain || !std::io::stdout().is_terminal();
    if let Some(log_path) = init_tracing(!use_plain) {
        // Announced BEFORE the TUI takes the screen; visible in scrollback.
        eprintln!("logging to {}", log_path.display());
    }
    let (model, api_key) = select_model(&args)?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

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
    // Project context (AGENTS.md/CLAUDE.md, eager) and skills (SKILL.md,
    // lazy catalog) come from the binary's install dir and the project cwd.
    let roots = cupel_coding_agent::resources::default_roots(&cwd);
    let context_files = cupel_coding_agent::resources::load_context_files(&roots);
    let skills = cupel_coding_agent::resources::discover_skills(&roots);

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
        &skills,
    );

    let mut options = AgentOptions::new(model.clone(), Arc::new(cupel_core::default_registry()));
    options.system_prompt = system_prompt;
    options.tools = tools;
    options.api_key = api_key;
    options.thinking_level = args.thinking;
    options.tool_execution = ToolExecutionMode::Parallel;
    options.session_id = Some(format!("cupel-{}", cupel_core::types::now_ms()));
    let agent = Agent::new(options);

    let meta = SessionMeta {
        model_name: model.name.clone(),
        provider: model.provider.as_str().to_string(),
        cwd: cwd.display().to_string(),
    };

    // ---- Pick a frontend ------------------------------------------------------
    // The TUI takes over the whole screen; that only makes sense on a real
    // terminal. Piped output (cupel < script, CI logs) gets plain mode.
    if use_plain {
        modes::plain::run(agent, &meta).await
    } else {
        modes::interactive::run(agent, meta)
            .await
            .map_err(|e| e.to_string())
    }
}
