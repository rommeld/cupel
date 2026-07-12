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

/// The distinct providers of the built-in catalog, in catalog order, each
/// with its first (= default) model. Drives `/provider` listing, switching,
/// and argument autocomplete.
#[must_use]
pub fn catalog_providers() -> Vec<(String, Model)> {
    let mut providers: Vec<(String, Model)> = Vec::new();
    for model in cupel_core::catalog::builtin_models() {
        let name = model.provider.as_str().to_string();
        if !providers.iter().any(|(existing, _)| *existing == name) {
            providers.push((name, model));
        }
    }
    providers
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
        let providers = catalog_providers();
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
}
