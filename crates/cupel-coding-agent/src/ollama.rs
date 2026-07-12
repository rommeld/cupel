//! Ollama auto-discovery: locally pulled models appear in the catalog
//! (and therefore `/model` autocomplete and `/provider`) automatically.
//!
//! One bounded GET to ollama's native `/api/tags` at startup; the models
//! themselves are then driven through ollama's OpenAI-compatible
//! `{host}/v1/chat/completions` endpoint by the existing
//! `openai-completions` provider - discovery is the only ollama-specific
//! code in the workspace.
//!
//! Fail-soft by design: ollama not running is the NORMAL case for most
//! users, so any failure (connection refused, timeout, bad JSON) logs at
//! debug and yields an empty list - the resources.rs warn-and-continue
//! idiom, one notch quieter.

use cupel_core::types::{Api, InputModality, Model, ModelCost, Provider};

/// How long the whole probe may take. Localhost answers in microseconds;
/// the budget only matters for a firewalled remote OLLAMA_HOST that
/// black-holes packets.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Discovered models claim a conservative 4096-token window - ollama's own
/// default context length. Understating merely compacts early; OVERSTATING
/// would make the server truncate the prompt silently, which corrupts tool
/// calls. Users who raise ollama's context pin the model in models.json.
const DEFAULT_CONTEXT_WINDOW: u64 = 4096;

/// The ollama endpoint: `OLLAMA_HOST` when set, else the local default.
#[must_use]
pub fn ollama_host() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map_or_else(
            || "http://localhost:11434".to_string(),
            |v| normalize_host(&v),
        )
}

/// OLLAMA_HOST is commonly scheme-less (`0.0.0.0:11434`, ollama's own
/// convention); default the scheme and drop a trailing slash so URL
/// concatenation stays predictable.
fn normalize_host(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Probe `{host}/api/tags` and map every pulled model into the catalog.
pub async fn discover(host: &str) -> Vec<Model> {
    let url = format!("{host}/api/tags");
    let fetch = async {
        let response = reqwest::Client::new().get(&url).send().await.ok()?;
        response.json::<serde_json::Value>().await.ok()
    };
    match tokio::time::timeout(PROBE_TIMEOUT, fetch).await {
        Ok(Some(json)) => models_from_tags(&json, host),
        Ok(None) | Err(_) => {
            tracing::debug!(url, "ollama not reachable - skipping discovery");
            Vec::new()
        }
    }
}

/// The pure mapping core: an `/api/tags` response into catalog entries.
/// Split from the fetch so tests need no HTTP mock (parameterized like
/// `resolve_config_home` in resources.rs).
#[must_use]
pub fn models_from_tags(json: &serde_json::Value, host: &str) -> Vec<Model> {
    let Some(tags) = json.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    tags.iter()
        .filter_map(|tag| tag.get("name")?.as_str())
        .map(|name| Model {
            id: name.to_string(),
            name: name.to_string(),
            api: Api::from(Api::OPENAI_COMPLETIONS),
            provider: Provider::from("ollama"),
            // The provider appends /chat/completions; /v1 is ollama's
            // OpenAI-compatible prefix.
            base_url: format!("{host}/v1"),
            // /api/tags doesn't say whether a model thinks; models.json
            // pinning is the override for reasoning-capable ones.
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            // Local inference is free - keeps /usage honest at $0.
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cached_read: 0.0,
                cached_write: 0.0,
            },
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_tokens: DEFAULT_CONTEXT_WINDOW,
            headers: None,
            // Safe-side flags for local servers: no key, and none of the
            // OpenAI-proprietary body fields that stricter clones
            // (llama-server) reject.
            compat: Some(serde_json::json!({
                "requiresApiKey": false,
                "supportsStore": false,
                "supportsDeveloperRole": false,
                "supportsStrictMode": false,
                "supportsReasoningEffort": false,
                "maxTokensField": "max_tokens",
            })),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_host_adds_scheme_and_trims_slash() {
        assert_eq!(normalize_host("0.0.0.0:11434"), "http://0.0.0.0:11434");
        assert_eq!(normalize_host("http://box:11434/"), "http://box:11434");
        assert_eq!(normalize_host("https://remote/"), "https://remote");
    }

    #[test]
    fn models_from_tags_maps_the_fixture() {
        // Shape of a real /api/tags response (fields we ignore elided).
        let json = serde_json::json!({
            "models": [
                {"name": "qwen3:8b", "size": 5_200_000_000_u64},
                {"name": "llama3.2:latest", "size": 2_000_000_000_u64},
            ]
        });
        let models = models_from_tags(&json, "http://localhost:11434");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "qwen3:8b");
        assert_eq!(models[0].base_url, "http://localhost:11434/v1");
        assert_eq!(models[0].api.as_str(), "openai-completions");
        assert_eq!(models[0].provider.as_str(), "ollama");
        assert_eq!(models[0].context_window, DEFAULT_CONTEXT_WINDOW);
        assert!(models[0].cost.input.abs() < f64::EPSILON, "local is free");
        assert_eq!(
            models[0].compat.as_ref().unwrap()["requiresApiKey"],
            serde_json::json!(false)
        );
        assert_eq!(models[1].id, "llama3.2:latest");
    }

    #[test]
    fn models_from_tags_tolerates_missing_or_odd_shapes() {
        assert!(models_from_tags(&serde_json::json!({}), "h").is_empty());
        assert!(models_from_tags(&serde_json::json!({"models": []}), "h").is_empty());
        // Entries without a name are skipped, not fatal.
        let json = serde_json::json!({"models": [{"size": 1}, {"name": "ok:latest"}]});
        assert_eq!(models_from_tags(&json, "h").len(), 1);
    }
}
