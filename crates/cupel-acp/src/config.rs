//! Process configuration, session working directories arrive over ACP.

use std::path::PathBuf;

use cupel_coding_agent::startup::ThinkingChoice;

#[derive(Clone, Default)]
pub struct Config {
    pub model: Option<String>,
    pub thinking: ThinkingChoice,
    pub home: Option<PathBuf>,
}

pub const HELP: &str = "usage: cupel-acp [--model <id>] [--thinking off|minimal|low|medium|high|xhigh|max]\n\nRuns ACP v1 over stdin/stdout. Configure credentials through Cupel's environment\nor home configuration. Session directories come from the ACP client.\n";

pub fn parse(args: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut config = Config::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--model" | "-m" => config.model = Some(args.next().ok_or("--model requires a value")?),
            "--thinking" | "-t" => {
                config.thinking = args.next().ok_or("--thinking requires a value")?.parse()?;
            }
            "--help" | "-h" => return Ok(None),
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    config.home = cupel_coding_agent::resources::config_home();
    Ok(Some(config))
}
