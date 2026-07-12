//! Provider credential resolution, shared by startup (`select_model` in
//! main.rs) and the runtime `/provider` + `/model` built-ins - one place
//! that knows which environment variable each provider reads.
//!
//! Note on `export`: keys entered at runtime CANNOT be written back into
//! the process environment - `std::env::set_var` is unsafe in edition 2024
//! (not thread-safe) and this workspace forbids unsafe code. Runtime keys
//! therefore live in the frontend's session state and take precedence over
//! the environment; exported variables remain the startup path.

use cupel_core::types::Model;

/// The environment variable a provider reads its API key from; `None` for
/// providers with their own credential mechanism (Bedrock's AWS chain).
#[must_use]
pub fn env_var_name(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "fireworks" => Some("FIREWORKS_API_KEY"),
        _ => None,
    }
}

/// The exported API key for a provider, if any.
#[must_use]
pub fn env_api_key(provider: &str) -> Option<String> {
    std::env::var(env_var_name(provider)?).ok()
}

/// Whether the AWS credential chain has anything to work with. (Bedrock
/// resolves credentials inside the provider; this only decides whether to
/// OFFER Bedrock as a usable default.)
#[must_use]
pub fn has_aws_credentials() -> bool {
    std::env::var("AWS_ACCESS_KEY_ID").is_ok() || std::env::var("AWS_PROFILE").is_ok()
}

/// The distinct providers of the given catalog, in catalog order, each
/// with its first (= default) model. Drives `/provider` listing, switching,
/// and argument autocomplete. Takes the MERGED catalog (SessionMeta.models)
/// so user-defined and discovered providers appear alongside built-ins.
#[must_use]
pub fn catalog_providers(models: &[Model]) -> Vec<(String, Model)> {
    let mut providers: Vec<(String, Model)> = Vec::new();
    for model in models {
        let name = model.provider.as_str().to_string();
        if !providers.iter().any(|(existing, _)| *existing == name) {
            providers.push((name, model.clone()));
        }
    }
    providers
}

/// Whether a model's endpoint takes no API key at all (compat
/// `requiresApiKey: false` - local servers like ollama/llama-server; see
/// cupel-core's CompletionsCompat).
#[must_use]
pub fn is_keyless(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("requiresApiKey"))
        .and_then(serde_json::Value::as_bool)
        == Some(false)
}

/// A provider counts as keyless when EVERY one of its models is - drives
/// the `/provider` status line and select_model's last-resort local
/// default. (A mixed provider still needs its key.)
#[must_use]
pub fn provider_is_keyless(models: &[Model], provider: &str) -> bool {
    let mut any = false;
    for model in models.iter().filter(|m| m.provider.as_str() == provider) {
        if !is_keyless(model) {
            return false;
        }
        any = true;
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names_match_the_documented_providers() {
        assert_eq!(env_var_name("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_var_name("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(env_var_name("fireworks"), Some("FIREWORKS_API_KEY"));
        // Bedrock has no key variable - the AWS chain handles it.
        assert_eq!(env_var_name("amazon-bedrock"), None);
        assert_eq!(env_var_name("unknown"), None);
    }

    #[test]
    fn catalog_providers_are_distinct_with_a_default_model() {
        let providers = catalog_providers(&cupel_core::catalog::builtin_models());
        assert!(providers.len() >= 4, "anthropic/openai/bedrock/fireworks");
        // Catalog order: anthropic first, and each provider appears once.
        assert_eq!(providers[0].0, "anthropic");
        let mut names: Vec<&str> = providers.iter().map(|(n, _)| n.as_str()).collect();
        names.dedup();
        assert_eq!(names.len(), providers.len(), "no duplicates");
        // The default model actually belongs to its provider.
        for (name, model) in &providers {
            assert_eq!(model.provider.as_str(), name);
        }
    }

    #[test]
    fn keyless_detection_reads_the_compat_flag() {
        let mut model = cupel_core::catalog::builtin_models().remove(0);
        assert!(!is_keyless(&model), "no compat = key required");
        model.compat = Some(serde_json::json!({"requiresApiKey": false}));
        assert!(is_keyless(&model));
        model.compat = Some(serde_json::json!({"requiresApiKey": true}));
        assert!(!is_keyless(&model));

        // A provider is keyless only when ALL of its models are.
        let mut keyless = cupel_core::catalog::builtin_models().remove(0);
        keyless.id = "local".into();
        keyless.provider = cupel_core::types::Provider::from("ollama");
        keyless.compat = Some(serde_json::json!({"requiresApiKey": false}));
        let mut keyed = keyless.clone();
        keyed.id = "local-2".into();
        keyed.compat = None;

        assert!(provider_is_keyless(
            std::slice::from_ref(&keyless),
            "ollama"
        ));
        assert!(!provider_is_keyless(&[keyless.clone(), keyed], "ollama"));
        assert!(
            !provider_is_keyless(&[keyless], "unknown"),
            "no models = not keyless"
        );
    }
}
