//! Dev-time model-catalog generator.
//!
//! Fetches <https://models.dev/api.json>, applies the curation tables in
//! curation.rs, validates, and writes the committed
//! crates/cupel-core/src/catalog.json that builtin_models() embeds via
//! include_str!. Run on demand:
//!
//!     cargo run -p cupel-coding-agent --bin generate-catalog
//!
//! It never runs at cupel runtime - the catalog is data checked into git.

mod curation;
mod models_dev;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cupel_core::types::{Api, CostTier, InputModality, Model, ModelCost, Provider};

use crate::curation::{
    Curated, CuratedProvider, MODELS_DEV_URL, OPENAI_CODEX_MODELS, PROVIDERS, Thinking,
};
use crate::models_dev::ProviderEntry;

// const MODELS_DEV_URL: &str = "https://models.dev/api.json";

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
    let raw = fetch(MODELS_DEV_URL).await?;
    let wanted: Vec<&str> = PROVIDERS.iter().map(|p| p.models_dev_id).collect();
    let catalog = models_dev::parse_wanted(&raw, &wanted)?;
    let mut models = build_models(PROVIDERS, &catalog)?;
    // Codex rides BEHIND the models.dev providers - pi appends its
    // codexModels after the fetched catalog the same way. Appended here
    // (not inside build_models) so the join stays a pure function of the
    // curation table.
    models.extend(openai_codex_models());
    validate(&models)?;
    print_summary(&models);

    let json = to_pretty_json(&models)?;
    // Round-trip self-check: the bytes we are about to commit must parse
    // back into the exact same models (catches serde asymmetries early).
    let reparsed: Vec<Model> =
        serde_json::from_str(&json).map_err(|error| format!("round-trip parse: {error}"))?;
    if reparsed != models {
        return Err("round-trip check failed: reparsed catalog differs".to_string());
    }

    let path = output_path();
    std::fs::write(&path, &json).map_err(|error| format!("write {}: {error}", path.display()))?;
    println!("wrote {} models to {}", models.len(), path.display());
    Ok(())
}

/// One bounded GET - mirrors the ollama probe's spirit: explicit
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

/// The pinned Codex rows as cupel Models - no models.dev join, the
/// curation table IS the data (see curation.rs for why).
fn openai_codex_models() -> Vec<Model> {
    OPENAI_CODEX_MODELS
        .iter()
        .map(|row| {
            let (input, output, cached_read, cached_write) = row.cost;
            let mut thinking_level_map = std::collections::BTreeMap::new();
            // pi pins minimal -> "low" (the backend has no minimal
            // effort). pi's xhigh/max identity pins are NOT copied:
            // under cupel's key-absence rule an xhigh entry would
            // DISABLE xhigh, and max is outside cupel's level scale.
            thinking_level_map.insert("minimal".to_string(), Some("low".to_string()));
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
                    // pi's withOpenAiLongContextPricing: past 272k prompt
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
                max_tokens: 128_000,
                headers: None,
                compat: Some(serde_json::json!({"requestModel": row.id})),
            }
        })
        .collect()
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
        context_window: entry.limit.context,
        max_tokens: entry.limit.output,
        headers: None,
        compat: row.compat.to_value(),
    })
}

/// models.dev knows text/image/pdf/audio/video; cupel's InputModality
/// only text/image - the rest is dropped (documentd deviation).
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

fn to_pretty_json(models: &[Model]) -> Result<String, String> {
    let mut json =
        serde_json::to_string_pretty(models).map_err(|error| format!("serialize: {error}"))?;
    // Comitted files end with a newline.
    json.push('\n');
    Ok(json)
}

/// Resolve crates/cupel-core/src/catalog.json relative to THIS crate's
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
        // pdf is dropped - cupel only models text and image input.
        assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
        assert!((model.cost.cached_write - 2.5).abs() < f64::EPSILON);
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_tokens, 128_000);
        // Budget thinking: no map at all.
        assert!(model.thinking_level_map.is_none());
        assert!(model.compat.is_none());
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
        let error = validate(&[good.clone(), broken, good.clone()]).unwrap_err();
        assert!(error.contains("duplicated id claude-sonnet-5"), "{error}");
        assert!(error.contains("contextWindow is 0"), "{error}");
        assert!(error.contains("not an http(s) URL"), "{error}");

        // The order contract: anything but sonnet-5 first is an error.
        let mut renamed = good;
        renamed.id = "claude-opus-5".to_string();
        let error = validate(&[renamed]).unwrap_err();
        assert!(error.contains("models[0]"), "{error}");
    }

    #[test]
    fn codex_rows_are_pinned_namespaced_and_tiered() {
        let models = openai_codex_models();
        assert_eq!(models.len(), 7, "pi's explicit codex list");
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
        // Spot checks against pi's generate-models.ts values.
        let sol = &models[0];
        assert_eq!(sol.id, "codex/gpt-5.6-sol", "first row = /provider default");
        let tiers = sol.cost.tiers.as_ref().expect("long-context tier");
        assert_eq!(tiers[0].context_over, 272_000);
        assert!((tiers[0].input - 10.0).abs() < f64::EPSILON, "5.0 x2");
        assert!((tiers[0].output - 45.0).abs() < f64::EPSILON, "30.0 x1.5");
        let spark = models.last().expect("seven rows");
        assert_eq!(spark.context_window, 128_000);
        assert!(spark.cost.tiers.is_none(), "spark has no long-context tier");
        assert_eq!(spark.input, vec![InputModality::Text], "spark is text-only");
    }
}
