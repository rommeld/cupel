//! Shared startup policy for terminal and protocol frontend.

use cupel_core::types::{Model, ThinkingLevel};

use crate::settings::{DEFAULT_PRESET, Preset, Settings};

/// A frontend override, distinct from the configured default.
#[derive(Debug, Clone, Copy, Default)]
pub enum ThinkingChoice {
    #[default]
    Default,
    Explicit(Option<ThinkingLevel>),
}

impl std::str::FromStr for ThinkingChoice {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let level = match value {
            "off" => None,
            "minimal" => Some(ThinkingLevel::Minimal),
            "low" => Some(ThinkingLevel::Low),
            "medium" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            "xhigh" => Some(ThinkingLevel::XHigh),
            "max" => Some(ThinkingLevel::Max),
            _ => {
                return Err(format!(
                    "unknown thinking level: {value}; expected off, minimal, low, medium, high, xhigh, or max"
                ));
            }
        };
        Ok(Self::Explicit(level))
    }
}

#[must_use]
pub fn thinking_level(
    choice: ThinkingChoice,
    model: &Model,
    preset: Option<&Preset>,
) -> Option<ThinkingLevel> {
    match choice {
        ThinkingChoice::Explicit(level) => level,
        ThinkingChoice::Default => match preset {
            Some(preset) => preset.thinking_level.level(),
            None => Some(
                if model.id == "codex/gpt-6-sol" && model.provider.as_str() == "openai-codex" {
                    ThinkingLevel::High
                } else {
                    ThinkingLevel::Medium
                },
            ),
        },
    }
}

/// Pick a model + API key from CLI args and the merged catalog (built-ins, models.json
/// layers, discovered local models). Credential knowledge lives in `providers.rs`,
/// shared with the TUI's `/provider` command; keys sources at startup: exported env
/// var, then ~/.cupel/settings.json. Precedence: `--model`> the `default`preset > a
/// ChatGPT login > the first provider with credentials > a keyless local model.
pub fn select_model(
    wanted: Option<&str>,
    catalog: &[Model],
    settings: &Settings,
    home: Option<&std::path::Path>,
) -> Result<(Model, Option<String>), String> {
    use crate::providers;

    if let Some(wanted) = wanted {
        let model = catalog
            .iter()
            .find(|m| m.id == wanted)
            .cloned()
            .ok_or_else(|| format!("unknown model: {wanted} (see --help for the list)"))?;
        let key = providers::resolve_api_key(model.provider.as_str(), settings);
        return Ok((model, key));
    }

    // The default preset is explicit configuration: an unknown model is an error like
    // a mistyped --model, never a silent start on something else. A default preset
    // that does not even parse is the same mistake.
    if let Some(reason) = &settings.invalid_default_preset {
        return Err(format!(
            "invalid default preset in {reason} (fix \"model\" in settings.json)"
        ));
    }
    if let Some(preset) = settings.presets.get(DEFAULT_PRESET) {
        let model = preset.find_model(catalog).cloned().ok_or_else(|| {
            format!(
                "unknown model in the default preset: {}/{} (fix \"model\" in settings.json; \
                    see --help for the list)",
                preset.provider, preset.model,
            )
        })?;
        let key = providers::resolve_api_key(model.provider.as_str(), settings);
        return Ok((model, key));
    }

    // A stored ChatGPT login makes Codex the default, regardless of where its models
    // occure in the catalog or which other keys are configured.
    if crate::auth::has_credential(home, "openai-codex")
        && let Some(model) = catalog
            .iter()
            .find(|m| m.provider.as_str() == "openai-codex")
    {
        return Ok((model.clone(), None));
    }

    // Otherwise, first provider with cloud credentials wins, in catalog order.
    // Bedrock carries on key through StreamOptions because the AWS chain
    // resolves inside the provider. Keyless local models fall through here (no env
    // var, and normally not settings entry), so a configured cloud key always beats a
    // merely-running ollama.
    for model in catalog {
        match model.provider.as_str() {
            "amazon-bedrock" if providers::has_aws_credentials() => {
                return Ok((model.clone(), None));
            }
            "openai-codex" => {} // OAuth only, never a settings key.
            provider => {
                if let Some(key) = providers::resolve_api_key(provider, settings) {
                    return Ok((model.clone(), Some(key)));
                }
            }
        }
    }
    // Pass 2: with no cloud credentials anywhere, a keyless local model (discovered
    // ollama, models.json entry) is the last resort before giving up.
    if let Some(model) = catalog.iter().find(|m| providers::is_keyless(m)) {
        return Ok((model.clone(), None));
    }
    Err(
        "no credentials found: set ANTHROPIC_API_KEY, OPENAI_API_KEY, FIREWORKS_API_KEY, \
        OPENTROUTER_API_KEY, or AWS credentials, add a key to ~/.cupel/settings.json, log in \
        with a ChatGPT subscription (/login openai-codex in the TUI), start a local server \
        (ollama / llama-server - see README 'Local models'), or start with an explicit \
        `--model <id>` and enter a key in the TUI via `/provider <name> <api-key>`"
            .to_string(),
    )
}
