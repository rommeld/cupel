//! Dev-time model-catalog generator.
//!
//! Uses the checked-in models_dev.json snapshot, applies curation.rs, validates,
//! and writes crates/cupel-core/src/catalog.json. --fetch refreshes the snapshot
//! from <https://models.dev/api.json>, keeping only curated models and consumed
//! fields. Run on demand:
//!
//!     cargo run -p cupel-coding-agent --bin generate-catalog
//!     cargo run -p cupel-coding-agent --bin generate-catalog -- --fetch
//!
//! It never runs at cupel runtime. The catalog is data checked into git.

mod curation;
mod models_dev;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cupel_core::types::{Api, CostTier, InputModality, Model, ModelCost, Provider};

use crate::curation::{
    Curated, CuratedProvider, MODELS_DEV_URL, OPENAI_CODEX_MODELS, PROVIDERS, Thinking,
    ULTRAFAST_BASES, ULTRAFAST_PRICE_FACTOR, Window,
};
use crate::models_dev::ProviderEntry;

// const MODELS_DEV_URL: &str = "https://models.dev/api.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Generate,
    Fetch,
    Help,
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::Generate),
        [flag] if flag == "--fetch" => Ok(Command::Fetch),
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        _ => Err("usage: generate-catalog [--fetch]".to_string()),
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("generate-catalog: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let use_fetch = match parse_command(&std::env::args().skip(1).collect::<Vec<_>>())? {
        Command::Generate => false,
        Command::Fetch => true,
        Command::Help => {
            println!(
                "usage: generate-catalog [--fetch]\n\nWithout --fetch, uses the checked-in models_dev.json snapshot."
            );
            return Ok(());
        }
    };
    let snapshot_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/generate_catalog/models_dev.json");
    let raw = if use_fetch {
        curated_snapshot(&fetch(MODELS_DEV_URL).await?)?
    } else {
        std::fs::read_to_string(&snapshot_path)
            .map_err(|error| format!("read {}: {error}", snapshot_path.display()))?
    };
    // Offline, "models.dev no longer lists ..." means the checked-in snapshot
    // lacks a row that curation.rs now wants, not that models.dev dropped it.
    let (models, json) = generate(&raw).map_err(|error| {
        if use_fetch {
            error
        } else {
            format!(
                "{error} (read from the checked-in snapshot {}; run with --fetch to refresh it)",
                snapshot_path.display()
            )
        }
    })?;
    print_summary(&models);
    // Validate everything before updating either checked-in artifact.
    if use_fetch {
        std::fs::write(&snapshot_path, raw)
            .map_err(|error| format!("write {}: {error}", snapshot_path.display()))?;
        println!("wrote {}", snapshot_path.display());
    }
    let path = output_path();
    std::fs::write(&path, &json).map_err(|error| format!("write {}: {error}", path.display()))?;
    println!("wrote {} models to {}", models.len(), path.display());
    Ok(())
}

fn generate(raw: &str) -> Result<(Vec<Model>, String), String> {
    let wanted: Vec<&str> = PROVIDERS.iter().map(|p| p.models_dev_id).collect();
    let catalog = models_dev::parse_wanted(raw, &wanted)?;
    let mut models = build_models(PROVIDERS, &catalog)?;
    // Codex rides behind the models.dev providers. Appended here
    // (not inside build_models) so the join stays a pure function of the
    // curation table.
    models.extend(openai_codex_models());
    let mut models = with_ultrafast_twins(models, ULTRAFAST_BASES)?;
    // JSON-valued fields must keep their historical ordering even when another
    // workspace member enables serde_json/preserve_order.
    for compat in models.iter_mut().filter_map(|model| model.compat.as_mut()) {
        compat.sort_all_objects();
    }
    validate(&models)?;

    let json = to_pretty_json(&models)?;
    // Round-trip self-check: the bytes we are about to commit must parse
    // back into the exact same models (catches serde asymmetries early).
    let reparsed: Vec<Model> =
        serde_json::from_str(&json).map_err(|error| format!("round-trip parse: {error}"))?;
    if reparsed != models {
        return Err("round-trip check failed: reparsed catalog differs".to_string());
    }
    Ok((models, json))
}

/// A deterministic, normalized subset: only curated models and fields represented
/// by the models.dev mirror. Unused upstream schema changes stay out of the snapshot.
fn curated_snapshot(raw: &str) -> Result<String, String> {
    let wanted: Vec<&str> = PROVIDERS.iter().map(|p| p.models_dev_id).collect();
    let catalog = models_dev::parse_wanted(raw, &wanted)?;
    let mut snapshot = BTreeMap::new();
    for provider in PROVIDERS {
        let entry = catalog
            .get(provider.models_dev_id)
            .ok_or_else(|| format!("provider {} missing in models.dev", provider.models_dev_id))?;
        let models = provider
            .models
            .iter()
            .map(|row| {
                let model = entry.model(provider.models_dev_id, row.id)?;
                let mut value = serde_json::to_value(model).map_err(|error| {
                    format!("serialize {}/{}: {error}", provider.models_dev_id, row.id)
                })?;
                // Keep snapshots deterministic with serde_json/preserve_order enabled.
                value.sort_all_objects();
                Ok((row.id.to_string(), value))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        snapshot.insert(provider.models_dev_id, ProviderEntry { models });
    }
    to_pretty_json(&snapshot)
}

/// One bounded GET, like the ollama probe: explicit
/// timeout, HTTP errors surfaced with the URL in the message.
async fn fetch(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("GET {url}: {error}"))?;
    response
        .text()
        .await
        .map_err(|error| format!("ready body of {url}: {error}"))
}

fn build_models(
    providers: &[CuratedProvider],
    catalog: &BTreeMap<String, ProviderEntry>,
) -> Result<Vec<Model>, String> {
    let mut models = Vec::new();
    for provider in providers {
        let entry = catalog.get(provider.models_dev_id).ok_or_else(|| {
            format!(
                "provider {} missing from the parsed catalog",
                provider.models_dev_id
            )
        })?;
        for row in provider.models {
            let source = entry.model(provider.models_dev_id, row.id)?;
            models.push(to_model(provider, row, &source)?);
        }
    }
    Ok(models)
}

/// The pinned Codex rows as cupel Models. The curation table is the data,
/// with no models.dev join (see curation.rs for why).
fn openai_codex_models() -> Vec<Model> {
    OPENAI_CODEX_MODELS
        .iter()
        .map(|row| {
            let (input, output, cached_read, cached_write) = row.cost;
            let has = |level: &str| row.levels.contains(&level);
            let mut thinking_level_map = std::collections::BTreeMap::new();
            if !has("none") {
                thinking_level_map.insert("off".to_string(), None);
            }
            if !has("minimal") {
                thinking_level_map.insert("minimal".to_string(), Some("low".to_string()));
            }
            for level in ["low", "medium", "high", "xhigh", "max"] {
                if !has(level) {
                    thinking_level_map.insert(level.to_string(), None);
                }
            }
            let mut compat = serde_json::json!({"requestModel": row.id});
            if !row.temperature {
                compat["supportsTemperature"] = serde_json::json!(false);
            }
            Model {
                id: format!("codex/{}", row.id),
                name: row.name.to_string(),
                api: Api::from(Api::OPENAI_CODEX_RESPONSES),
                provider: Provider::from(Provider::OPENAI_CODEX),
                base_url: curation::OPENAI_CODEX_BASE_URL.to_string(),
                reasoning: true,
                thinking_level_map: Some(thinking_level_map),
                input: if row.vision {
                    vec![InputModality::Text, InputModality::Image]
                } else {
                    vec![InputModality::Text]
                },
                cost: ModelCost {
                    input,
                    output,
                    cached_read,
                    cached_write,
                    // Past 272k prompt
                    // tokens the whole request reprices at input x2,
                    // output x1.5, cache x2.
                    tiers: row.long_context_tier.then(|| {
                        vec![CostTier {
                            context_over: 272_000,
                            input: input * 2.0,
                            output: output * 1.5,
                            cached_read: cached_read * 2.0,
                            cached_write: cached_write * 2.0,
                        }]
                    }),
                },
                context_window: row.context_window,
                max_context_window: row.max_context_window,
                max_tokens: 128_000,
                headers: None,
                compat: Some(compat),
            }
        })
        .collect()
}

/// Put an Ultrafast twin right behind every row named in `bases`, so /model lists both
/// speeds of a model next to each other.
fn with_ultrafast_twins(models: Vec<Model>, bases: &[&str]) -> Result<Vec<Model>, String> {
    if let Some(missing) = bases
        .iter()
        .find(|&&id| !models.iter().any(|model| model.id == id))
    {
        return Err(format!("Ultrafast base {missing} is not in the catalog."));
    }
    Ok(models
        .into_iter()
        // flat_map turns each row into one or two rows: the row itself, then its twin
        // when `then` produced one (`Option` iterates as zero or one items, so `chain`
        // appends nothing for None).
        .flat_map(|model| {
            let twin = bases
                .contains(&model.id.as_str())
                .then(|| ultrafast_twin(&model));
            std::iter::once(model).chain(twin)
        })
        .collect())
}

/// The base row on the Ultrafast tier: the same model on the wire with the same limits
/// and effort scale, under its own id, name and prices.
fn ultrafast_twin(base: &Model) -> Model {
    let mut compat = base.compat.clone().unwrap_or_else(|| serde_json::json!({}));
    // Codex rows already carry the backend name ("gpt-6.1-sol" for
    // "codex/gpt-6.1-sol"); a plain openai row's id is its API name.
    if compat.get("requestModel").is_none() {
        compat["requestModel"] = serde_json::json!(base.id);
    }
    compat["serviceTier"] = serde_json::json!("ultrafast");
    Model {
        id: format!("{}-ultrafast", base.id),
        name: format!("{} Ultrafast", base.name),
        cost: scaled_cost(&base.cost, ULTRAFAST_PRICE_FACTOR),
        compat: Some(compat),
        ..base.clone()
    }
}

/// Every price times `factor`, long-context tiers included.
fn scaled_cost(cost: &ModelCost, factor: f64) -> ModelCost {
    // f64 is binary, so 0.1 has no exact representation and
    // 0.1 * 6.0 == 0.6000000000000001. Rounding to a millionth of a
    // dollar keeps catalog.json at 0.6 without changing any real price.
    let price = |usd: f64| (usd * factor * 1e6).round() / 1e6;
    ModelCost {
        input: price(cost.input),
        output: price(cost.output),
        cached_read: price(cost.cached_read),
        cached_write: price(cost.cached_write),
        tiers: cost.tiers.as_ref().map(|tiers| {
            tiers
                .iter()
                .map(|tier| CostTier {
                    context_over: tier.context_over,
                    input: price(tier.input),
                    output: price(tier.output),
                    cached_read: price(tier.cached_read),
                    cached_write: price(tier.cached_write),
                })
                .collect()
        }),
    }
}

/// Merge on curation row with its model.dev entry into a cupel Model.
fn to_model(
    provider: &CuratedProvider,
    row: &Curated,
    entry: &models_dev::ModelEntry,
) -> Result<Model, String> {
    let cost = entry.cost.as_ref().ok_or_else(|| {
        format!(
            "{}/{} has no cost data on models.dev",
            provider.cupel_id, row.id
        )
    })?;
    let thinking_level_map = match &row.thinking {
        Thinking::Budget => None,
        Thinking::FromEffort => {
            models_dev::thinking_level_map_from_effort(&entry.reasoning_options)
        }
        Thinking::Explicit(pairs) => Some(
            pairs
                .iter()
                .copied()
                .map(|(level, effort)| (level.to_string(), effort.map(str::to_string)))
                .collect(),
        ),
    };
    let input = input_modalities(&entry.modalities.input);
    if input.is_empty() {
        return Err(format!(
            "{}/{} has no text/iamge input modality",
            provider.cupel_id, row.id
        ));
    }
    let (context_window, max_context_window) = match row.window {
        Window::ModelsDev => (entry.limit.context, None),
        Window::PriceTier => {
            // The lowest price-tier threshold is the planning window; a
            // PriceTier row whose models.dev entry lists no tier is a
            // curation error, not a silent fallback to the full window.
            let threshold = cost
                .tiers
                .iter()
                .map(|tier| tier.tier.size)
                .min()
                .ok_or_else(|| {
                    format!(
                        "{}/{} is curated as Window::PriceTier but models.dev lists no price tier",
                        provider.cupel_id, row.id
                    )
                })?;
            (
                threshold,
                Some(entry.limit.input.unwrap_or(entry.limit.context)),
            )
        }
    };
    Ok(Model {
        id: row.id.to_string(),
        name: row.rename.unwrap_or(entry.name.as_str()).to_string(),
        api: Api::from(row.api),
        provider: Provider::from(provider.cupel_id),
        base_url: row.base_url.to_string(),
        reasoning: entry.reasoning,
        thinking_level_map,
        input,
        cost: ModelCost {
            input: cost.input,
            output: cost.output,
            cached_read: cost.cache_read,
            cached_write: cost.cache_write,
            tiers: (!cost.tiers.is_empty()).then(|| {
                cost.tiers
                    .iter()
                    .map(|tier| CostTier {
                        context_over: tier.tier.size,
                        input: tier.input,
                        output: tier.output,
                        cached_read: tier.cache_read,
                        cached_write: tier.cache_write,
                    })
                    .collect()
            }),
        },
        context_window,
        max_context_window,
        max_tokens: entry.limit.output,
        headers: None,
        compat: with_temperature_knob(row.compat.to_value(), row.api, entry.temperature),
    })
}

/// models.dev's `temperature: false` becomdes the OpenAI-family compat
/// knob `supportsTemperature: false`. Anthropic rows keep their explicit
/// templates.
fn with_temperature_knob(
    compat: Option<serde_json::Value>,
    api: &str,
    supports_temperature: bool,
) -> Option<serde_json::Value> {
    if supports_temperature || !matches!(api, Api::OPENAI_RESPONSES | Api::OPENAI_COMPLETIONS) {
        return compat;
    }
    let mut compat = compat.unwrap_or_else(|| serde_json::json!({}));
    compat["supportsTemperature"] = serde_json::json!(false);
    Some(compat)
}

/// models.dev knows text/image/pdf/audio/video; cupel's InputModality
/// only text/image. The rest is dropped (documented deviation).
fn input_modalities(raw: &[String]) -> Vec<InputModality> {
    let mut out = Vec::new();
    for modality in raw {
        match modality.as_str() {
            "text" => out.push(InputModality::Text),
            "image" => out.push(InputModality::Image),
            _ => {}
        }
    }
    out
}

fn print_summary(models: &[Model]) {
    println!(
        "{:<55} {:>7} {:>7} {:>11} {:>9}",
        "id", "in$/M", "out$/M", "context", "maxOut"
    );
    for model in models {
        println!(
            "{:<55} {:>7.2} {:>7.2} {:>11} {:>9}",
            model.id, model.cost.input, model.cost.output, model.context_window, model.max_tokens
        );
    }
}

fn to_pretty_json(value: &(impl serde::Serialize + ?Sized)) -> Result<String, String> {
    let mut json =
        serde_json::to_string_pretty(value).map_err(|error| format!("serialize: {error}"))?;
    // Comitted files end with a newline.
    json.push('\n');
    Ok(json)
}

/// Resolve crates/cupel-core/src/catalog.json relative to this crate's
/// manifest, so the generator works from any working directory.
fn output_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../cupel-core/src/catalog.json")
}

fn validate(models: &[Model]) -> Result<(), String> {
    let known_apis = [
        Api::ANTHROPIC_MESSAGES,
        Api::OPENAI_RESPONSES,
        Api::OPENAI_COMPLETIONS,
        Api::OPENAI_CODEX_RESPONSES,
        Api::BEDROCK_CONVERSE_STREAM,
    ];
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    for model in models {
        if !seen.insert(model.id.as_str()) {
            errors.push(format!("duplicated id {}", model.id));
        }
        if model.context_window == 0 {
            errors.push(format!("{}: contextWindow is 0", model.id));
        }
        if model.max_tokens == 0 {
            errors.push(format!("{}: maxTokens is 0", model.id));
        }
        if let Some(ceiling) = model.max_context_window
            && ceiling < model.context_window
        {
            errors.push(format!(
                "{}: maxContextWindow {ceiling} is below contextWindow {}",
                model.id, model.context_window,
            ));
        }
        if !known_apis.contains(&model.api.as_str()) {
            errors.push(format!("{}: unkown api {}", model.id, model.api));
        }
        let cost = &model.cost;
        if cost.input < 0.0
            || cost.output < 0.0
            || cost.cached_read < 0.0
            || cost.cached_write < 0.0
        {
            errors.push(format!("{}: negative cost", model.id));
        }
        if model.provider.as_str() == Provider::AMAZON_BEDROCK {
            if !model.base_url.is_empty() {
                errors.push(format!(
                    "{}: bedrock baseUrl must stay empty (the SDK derives it)",
                    model.id
                ));
            }
        } else if !model.base_url.starts_with("http") {
            errors.push(format!(
                "{}: baseUrl {:?} is not an http(s) URL",
                model.id, model.base_url
            ));
        }
    }
    // The catalog-order contract: fixtures across the workspace take
    // builtin_models().remove(0) as "a plain Anthropic model", and the
    // /provider default is first-in-order.
    match models.first() {
        Some(first)
            if first.id == "claude-sonnet-5" && first.provider.as_str() == Provider::ANTHROPIC => {}
        _ => errors.push(
            "models[0] must be anthropic/claude-sonnet-5 (workspace test fixtures rely on it)"
                .to_string(),
        ),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("validation failed:\n  - {}", errors.join("\n  - ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::Compat;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn checked_in_snapshot_holds_exactly_the_curated_subset() {
        // A row removed from curation.rs, or a hand-edited field, would
        // otherwise linger in the snapshot until the next --fetch diff.
        let source = include_str!("models_dev.json");
        assert_eq!(
            curated_snapshot(source).expect("the snapshot covers every curated row"),
            source,
            "stale snapshot: run cargo run -p cupel-coding-agent --bin generate-catalog -- --fetch"
        );
    }

    #[test]
    fn checked_in_catalog_is_generated_from_snapshot_and_curation() {
        let (models, json) = generate(include_str!("models_dev.json"))
            .expect("snapshot must contain all curated models; refresh with --fetch if needed");
        assert_eq!(
            models.len(),
            PROVIDERS.iter().map(|p| p.models.len()).sum::<usize>()
                + OPENAI_CODEX_MODELS.len()
                + ULTRAFAST_BASES.len()
        );
        assert_eq!(
            json,
            include_str!("../../../../cupel-core/src/catalog.json"),
            "catalog drift: run cargo run -p cupel-coding-agent --bin generate-catalog"
        );
    }

    #[test]
    fn snapshot_keeps_only_curated_models_and_consumed_fields() {
        let source = include_str!("models_dev.json");
        let mut upstream: serde_json::Value = serde_json::from_str(source).unwrap();
        upstream["unused-provider"] = serde_json::json!({"models": "bad schema"});
        upstream["anthropic"]["models"]["unused-model"] = serde_json::json!({"unknown": 42});
        upstream["anthropic"]["models"]["claude-sonnet-5"]["unknown"] = serde_json::json!(42);
        let snapshot = curated_snapshot(&upstream.to_string()).unwrap();
        assert_eq!(snapshot, curated_snapshot(source).unwrap());
        assert_eq!(generate(&snapshot).unwrap(), generate(source).unwrap());
        upstream["anthropic"]["models"]
            .as_object_mut()
            .unwrap()
            .remove("claude-sonnet-5");
        assert!(
            curated_snapshot(&upstream.to_string())
                .unwrap_err()
                .contains("claude-sonnet-5")
        );
    }

    #[test]
    fn fetch_is_explicit_and_unknown_arguments_are_rejected() {
        assert_eq!(parse_command(&[]).unwrap(), Command::Generate);
        assert_eq!(
            parse_command(&["--fetch".to_string()]).unwrap(),
            Command::Fetch
        );
        assert_eq!(
            parse_command(&["--help".to_string()]).unwrap(),
            Command::Help
        );
        assert!(parse_command(&["--offline".to_string()]).is_err());
        assert!(parse_command(&["--fetch".to_string(), "extra".to_string()]).is_err());
    }

    fn sonnet_entry() -> models_dev::ModelEntry {
        serde_json::from_value(serde_json::json!({
            "name": "Claude Sonnet 5",
            "reasoning": true,
            "reasoning_options": [
                {"type": "toggle"},
                {"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]}
            ],
            "modalities": {"input": ["text", "image", "pdf"]},
            "cost": {"input": 2.0, "output": 10.0, "cache_read": 0.2, "cache_write": 2.5},
            "limit": {"context": 1_000_000, "output": 128_000}
        }))
        .expect("fixture entry parses")
    }

    const TEST_ROW: Curated = Curated {
        id: "claude-sonnet-5",
        rename: None,
        api: Api::ANTHROPIC_MESSAGES,
        base_url: "https://api.anthropic.com",
        thinking: Thinking::Budget,
        compat: Compat::None,
        window: Window::ModelsDev,
    };
    const TEST_PROVIDER: CuratedProvider = CuratedProvider {
        models_dev_id: "anthropic",
        cupel_id: "anthropic",
        models: &[TEST_ROW],
    };

    #[test]
    fn to_model_maps_the_models_dev_fields() {
        let model = to_model(&TEST_PROVIDER, &TEST_ROW, &sonnet_entry()).expect("maps");
        assert_eq!(model.id, "claude-sonnet-5");
        assert_eq!(model.name, "Claude Sonnet 5");
        assert_eq!(model.api.as_str(), Api::ANTHROPIC_MESSAGES);
        assert_eq!(model.provider.as_str(), "anthropic");
        // pdf is dropped because cupel only models text and image input.
        assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
        assert_close(model.cost.input, 2.0);
        assert_close(model.cost.output, 10.0);
        assert_close(model.cost.cached_read, 0.2);
        assert!((model.cost.cached_write - 2.5).abs() < f64::EPSILON);
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_tokens, 128_000);
        // Budget thinking: no map at all.
        assert!(model.thinking_level_map.is_none());
        assert!(model.compat.is_none());
    }

    /// GPT-6 Astra exactly as models.dev lists it (2026-09-06).
    fn astra_entry() -> models_dev::ModelEntry {
        serde_json::from_value(serde_json::json!({
            "name": "GPT-6 Astra",
            "reasoning": true,
            "reasoning_options": [
                {"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]}
            ],
            "temperature": false,
            "modalities": {"input": ["text", "image", "pdf"]},
            "cost": {
                "input": 10, "output": 50, "cache_read": 1, "cache_write": 12.5,
                "tiers": [{"input": 20, "output": 75, "cache_read": 2, "cache_write": 25,
                           "tier": {"type": "context", "size": 272_000}}]
            },
            "limit": {"context": 1_050_000, "input": 922_000, "output": 128_000}
        }))
        .expect("fixture entry parses")
    }

    const ASTRA_ROW: Curated = Curated {
        id: "gpt-6-astra",
        rename: None,
        api: Api::OPENAI_RESPONSES,
        base_url: "https://api.openai.com/v1",
        thinking: Thinking::FromEffort,
        compat: Compat::None,
        window: Window::PriceTier,
    };
    const OPENAI_PROVIDER: CuratedProvider = CuratedProvider {
        models_dev_id: "openai",
        cupel_id: "openai",
        models: &[ASTRA_ROW],
    };

    #[test]
    fn price_tier_rows_plan_against_the_tier_threshold() {
        let model = to_model(&OPENAI_PROVIDER, &ASTRA_ROW, &astra_entry()).expect("maps");
        // 272k planning window (the price tier), 922k opt-in ceiling (the
        // documented max input), tier prices carried along.
        assert_eq!(model.context_window, 272_000);
        assert_eq!(model.max_context_window, Some(922_000));
        assert_eq!(model.max_tokens, 128_000);
        assert_eq!(
            model.cost.tiers.as_ref().expect("tier")[0].context_over,
            272_000
        );
        let cost = &model.cost;
        let tier = &cost.tiers.as_ref().expect("tier")[0];
        for (actual, expected) in [
            (cost.input, 10.0),
            (cost.output, 50.0),
            (cost.cached_read, 1.0),
            (cost.cached_write, 12.5),
            (tier.input, 20.0),
            (tier.output, 75.0),
            (tier.cached_read, 2.0),
            (tier.cached_write, 25.0),
        ] {
            assert_close(actual, expected);
        }
        // The effort scale: off/minimal disabled, xhigh/max kept by omission.
        let map = model.thinking_level_map.expect("map");
        assert_eq!(map.get("off"), Some(&None));
        assert_eq!(map.get("minimal"), Some(&None));
        assert!(!map.contains_key("xhigh") && !map.contains_key("max"));
        // temperature: false -> the compat knob, created from nothing.
        assert_eq!(
            model.compat,
            Some(serde_json::json!({"supportsTemperature": false}))
        );

        // A PriceTier row without a tier on models.dev is a named error.
        let mut tierless = astra_entry();
        tierless.cost.as_mut().expect("cost").tiers.clear();
        let error = to_model(&OPENAI_PROVIDER, &ASTRA_ROW, &tierless).unwrap_err();
        assert!(error.contains("openai/gpt-6-astra"), "{error}");
        assert!(error.contains("no price tier"), "{error}");
    }

    #[test]
    fn missing_cost_is_a_named_error() {
        let entry: models_dev::ModelEntry =
            serde_json::from_value(serde_json::json!({"name": "Painter", "cost": null}))
                .expect("sparse entry parses");
        let error = to_model(&TEST_PROVIDER, &TEST_ROW, &entry).unwrap_err();
        assert!(error.contains("anthropic/claude-sonnet-5"), "{error}");
        assert!(error.contains("no cost data"), "{error}");
    }

    #[test]
    fn build_models_walks_the_curation_table() {
        let raw = serde_json::json!({
            "anthropic": {"models": {"claude-sonnet-5": {
                "name": "Claude Sonnet 5",
                "reasoning": true,
                "modalities": {"input": ["text"]},
                "cost": {"input": 2.0, "output": 10.0, "cache_read": 0.2, "cache_write": 2.5},
                "limit": {"context": 1_000_000, "output": 128_000}
            }}}
        })
        .to_string();
        let catalog = models_dev::parse_wanted(&raw, &["anthropic"]).expect("parses");
        let models = build_models(&[TEST_PROVIDER], &catalog).expect("builds");
        assert_eq!(models.len(), 1);
        // A curated id models.dev dropped fails loudly, naming the fix.
        let gone = models_dev::parse_wanted(r#"{"anthropic": {"models": {}}}"#, &["anthropic"])
            .expect("parses");
        let error = build_models(&[TEST_PROVIDER], &gone).unwrap_err();
        assert!(error.contains("update curation.rs"), "{error}");
    }

    #[test]
    fn validate_collects_every_violation() {
        let good = to_model(&TEST_PROVIDER, &TEST_ROW, &sonnet_entry()).expect("maps");

        let mut broken = good.clone();
        broken.context_window = 0;
        broken.base_url = "not-a-url".to_string();
        let mut inverted = good.clone();
        inverted.id = "inverted".to_string();
        inverted.max_context_window = Some(inverted.context_window - 1);
        let error = validate(&[good.clone(), broken, good.clone(), inverted]).unwrap_err();
        assert!(error.contains("duplicated id claude-sonnet-5"), "{error}");
        assert!(error.contains("contextWindow is 0"), "{error}");
        assert!(error.contains("not an http(s) URL"), "{error}");
        assert!(error.contains("inverted: maxContextWindow"), "{error}");

        // The order contract: anything but sonnet-5 first is an error.
        let mut renamed = good;
        renamed.id = "claude-opus-5".to_string();
        let error = validate(&[renamed]).unwrap_err();
        assert!(error.contains("models[0]"), "{error}");
    }

    #[test]
    fn codex_rows_are_pinned_namespaced_and_tiered() {
        let models = openai_codex_models();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [
                "codex/gpt-6-sol",
                "codex/gpt-6.1-sol",
                "codex/gpt-6.1-terra",
                "codex/gpt-6-astra",
                "codex/gpt-6-luna",
            ]
        );
        for model in &models {
            // The id namespacing contract the provider's wire_model undoes.
            let request_model = model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("requestModel"))
                .and_then(serde_json::Value::as_str)
                .expect("every codex row pins requestModel");
            assert_eq!(model.id, format!("codex/{request_model}"));
            assert_eq!(model.api.as_str(), Api::OPENAI_CODEX_RESPONSES);
            assert_eq!(model.provider.as_str(), Provider::OPENAI_CODEX);
        }
        assert_eq!(
            models[0].id, "codex/gpt-6-sol",
            "first row = /provider default"
        );
        // Spot checks against the pinned catalog values.
        let astra = models
            .iter()
            .find(|m| m.id == "codex/gpt-6-astra")
            .expect("astra row");
        assert_eq!(astra.context_window, 272_000);
        assert_eq!(
            astra.max_context_window,
            Some(872_000),
            "Codex CLI's ceiling"
        );
        let tiers = astra.cost.tiers.as_ref().expect("long-context tier");
        assert_eq!(tiers[0].context_over, 272_000);
        assert!((tiers[0].input - 20.0).abs() < f64::EPSILON, "10.0 x2");
        assert!((tiers[0].output - 75.0).abs() < f64::EPSILON, "50.0 x1.5");
        let astra_map = astra.thinking_level_map.as_ref().expect("map");
        assert!(!astra_map.contains_key("max"), "max listed -> key absent");
        assert_eq!(
            astra
                .compat
                .as_ref()
                .and_then(|c| c.get("supportsTemperature")),
            Some(&serde_json::json!(false))
        );
        // GPT-6.1 Sol and Terra plan past their price tier:
        // the window is Codex's 500k default, yet turns above 272k still
        // show the tier's API-equivalent prices.
        for id in ["codex/gpt-6.1-sol", "codex/gpt-6.1-terra"] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert_eq!(model.context_window, 500_000, "{id}");
            assert_eq!(model.max_context_window, Some(872_000), "{id}");
            let tiers = model.cost.tiers.as_ref().expect("long-context tier");
            assert_eq!(tiers[0].context_over, 272_000, "{id}");
        }
    }

    #[test]
    fn ultrafast_twins_follow_their_base_rows() {
        // A small catalog: the Astra fixture stands in for a plain openai
        // row (no requestModel yet), the pinned rows for Codex.
        let mut models =
            vec![to_model(&OPENAI_PROVIDER, &ASTRA_ROW, &astra_entry()).expect("maps")];
        models.extend(openai_codex_models());
        let models =
            with_ultrafast_twins(models, &["gpt-6-astra", "codex/gpt-6.1-sol"]).expect("bases");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids[..5],
            [
                "gpt-6-astra",
                "gpt-6-astra-ultrafast",
                "codex/gpt-6-sol",
                "codex/gpt-6.1-sol",
                "codex/gpt-6.1-sol-ultrafast",
            ]
        );

        // The openai twin gets requestModel from the base id.
        assert_eq!(models[1].name, "GPT-6 Astra Ultrafast");
        assert_eq!(
            models[1].compat,
            Some(serde_json::json!({
                "supportsTemperature": false,
                "requestModel": "gpt-6-astra",
                "serviceTier": "ultrafast",
            }))
        );

        // The Codex twin keeps the backend name, not its namespaced id.
        let (base, twin) = (&models[3], &models[4]);
        assert_eq!(
            twin.compat,
            Some(serde_json::json!({
                "requestModel": "gpt-6.1-sol",
                "serviceTier": "ultrafast",
                "supportsTemperature": false,
            }))
        );
        // OpenAI's Ultrafast table, both context tiers. Comparing whole
        // structs works because scaled_cost rounds: 0.1 * 6.0 alone
        // would be 0.6000000000000001, not the 0.6 written here.
        assert_eq!(
            twin.cost,
            ModelCost {
                input: 12.0,
                output: 60.0,
                cached_read: 0.6,
                cached_write: 15.0,
                tiers: Some(vec![CostTier {
                    context_over: 272_000,
                    input: 24.0,
                    output: 90.0,
                    cached_read: 1.2,
                    cached_write: 30.0,
                }]),
            }
        );
        // Everything else is the base row.
        assert_eq!(twin.api, base.api);
        assert_eq!(twin.provider, base.provider);
        assert_eq!(twin.base_url, base.base_url);
        assert_eq!(twin.thinking_level_map, base.thinking_level_map);
        assert_eq!(twin.context_window, base.context_window);
        assert_eq!(twin.max_context_window, base.max_context_window);

        // A base that is not in the catalog is a named error.
        let error = with_ultrafast_twins(Vec::new(), &["gpt-6.1-sol"]).unwrap_err();
        assert!(error.contains("gpt-6.1-sol"), "{error}");
        assert!(error.contains("not in the catalog"), "{error}");
    }
}
