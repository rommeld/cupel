//! The merged model catalog: built-ins + user-defined `models.json`
//! layers + discovered local (ollama) models.
//!
//! `cupel_core::catalog::builtin_models()` is a hardcoded list; this module
//! layers user configuration over it so new models, proxies, and local
//! OpenAI-compatible endpoints never require recompiling. Layer order (later
//! wins on id collision, mirroring prompt-template precedence):
//!
//! 1. built-in catalog,
//! 2. `~/.cupel/models.json`,
//! 3. `<cwd>/.cupel/models.json` (overrides/credential-bearing rows require
//!    explicit project trust),
//! 4. ollama auto-discovery (lowest: an explicit entry always beats a
//!    discovered one. This is the user's override channel for context
//!    window, reasoning, and compat flags).
//!
//! The catalog is resolved once at startup in `main::run()` (discovery is a
//! network call; the TUI's key handlers are synchronous) and threaded to
//! the frontends via `SessionMeta.models`.

use std::path::Path;

use cupel_core::types::Model;

use crate::settings::Settings;

/// Parse one `models.json`: a JSON array of Model descriptors in the
/// workspace-wide camelCase serde form (`baseUrl`, `contextWindow`,
/// `maxTokens`, ...). A missing file is simply an empty layer; a malformed
/// file is an error the caller must surface, unlike optional context
/// files, a config the user wrote by hand deserves a visible failure.
pub fn load_models_file(path: &Path) -> Result<Vec<Model>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&content).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

/// The user layers in precedence order: cupel home first, project second.
/// Parse errors are collected as warnings and the broken layer is skipped,
/// never aborting startup. Reuse the loaded home settings for credential
/// checks so a malformed settings file is reported only once.
#[must_use]
pub fn load_user_models(
    home: Option<&Path>,
    cwd: &Path,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Vec<Vec<Model>> {
    let mut layers = Vec::new();
    if let Some(home) = home {
        layers.push(load_layer(&home.join("models.json"), warnings));
    }
    let project = load_layer(&cwd.join(".cupel/models.json"), warnings);
    if crate::project_trust::is_trusted(home, cwd) {
        layers.push(project);
    } else {
        let mut known = cupel_core::catalog::builtin_models();
        known.extend(layers.iter().flatten().cloned());
        let auth = crate::auth::load_auth(home);
        layers.push(
            project
                .into_iter()
                .filter(|model| {
                    let allowed = untrusted_model_allowed(model, &known, settings, &auth);
                    if !allowed {
                        warnings.push(format!(
                            "warning: ignoring project model {}: explicit project trust required",
                            model.id
                        ));
                    }
                    allowed
                })
                .collect(),
        );
    }
    layers
}

fn load_layer(path: &Path, warnings: &mut Vec<String>) -> Vec<Model> {
    match load_models_file(path) {
        Ok(models) => models,
        Err(e) => {
            warnings.push(format!("warning: ignoring models file: {e}"));
            Vec::new()
        }
    }
}

/// Only new, keyless loopback Completions entries on non-credential
/// providers are safe without trust. Check credential *capability*, not just today's
/// exported keys: /provider and /login can add credentials later. In
/// particular, a forged keyless flag must not bypass OAuth or AWS auth.
fn untrusted_model_allowed(
    model: &Model,
    known: &[Model],
    settings: &crate::settings::Settings,
    auth: &std::collections::BTreeMap<String, crate::auth::StoredCredential>,
) -> bool {
    let provider = model.provider.as_str();
    crate::providers::is_keyless(model)
        && model.api.as_str() == cupel_core::types::Api::OPENAI_COMPLETIONS
        && is_loopback_endpoint(&model.base_url)
        && !known.iter().any(|m| m.id == model.id)
        && !known
            .iter()
            .any(|m| m.provider == model.provider && !crate::providers::is_keyless(m))
        && crate::providers::env_var_name(provider).is_none()
        && provider != "openai-codex"
        && provider != "amazon-bedrock"
        && !settings.providers.contains_key(provider)
        && !auth.contains_key(provider)
}

fn is_loopback_endpoint(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// Merge catalog layers. An id collision replaces the earlier entry in
/// place (keeping its position, so each provider's first model stays the
/// `/provider` default); new ids append. Within one layer
/// the same rule applies, so a duplicated id in a single file is
/// last-wins.
#[must_use]
pub fn merge_models(layers: Vec<Vec<Model>>) -> Vec<Model> {
    let mut merged: Vec<Model> = Vec::new();
    for layer in layers {
        for model in layer {
            match merged.iter_mut().find(|m| m.id == model.id) {
                Some(existing) => *existing = with_context_ceiling(existing, model),
                None => merged.push(model),
            }
        }
    }
    merged
}

/// The catalog's context ceiling survives an override (Codex CLI's
/// with_config_overrides does the same clamp): a replacement row that
/// names no maxContextWindow inherits the replaced row's, and its
/// contextWindow is clamped to that ceiling. A row that pins its own
/// ceiling owns it
fn with_context_ceiling(existing: &Model, mut replacement: Model) -> Model {
    if replacement.max_context_window.is_none() {
        replacement.max_context_window = existing.max_context_window;
    }
    if let Some(ceiling) = replacement.max_context_window {
        replacement.context_window = replacement.context_window.min(ceiling);
    }
    replacement
}

/// Drop entries whose `api` has no registered provider implementation.
/// they would only fail at request time. Guards the same invariant the
/// built-in catalog tests enforce ("every model has a registered
/// provider"), extended to user input: warn and skip, never abort.
///
/// Kept rows are also checked against their provider's compat knobs: a key
/// with the wrong type falls back to its default at request time, so the
/// user sees a warning here instead of a mysterious behavior change.
#[must_use]
pub fn filter_registered(
    models: Vec<Model>,
    registry: &cupel_core::provider::Registry,
    warnings: &mut Vec<String>,
) -> Vec<Model> {
    models
        .into_iter()
        .filter(|model| {
            let Some(provider) = registry.get(model.api.as_str()) else {
                tracing::warn!(
                    model = %model.id,
                    api = %model.api.as_str(),
                    "skipping model: no provider implements this api"
                );
                warnings.push(format!(
                    "warning: skipping model {} - no provider implements api \"{}\"",
                    model.id,
                    model.api.as_str()
                ));
                return false;
            };
            for problem in provider.compat_problems(model) {
                warnings.push(format!(
                    "warning: model {} ignores compat setting {problem}",
                    model.id
                ));
            }
            true
        })
        .collect()
}

/// The full startup catalog: built-ins, user layers, then ollama
/// discovery for ids not already defined. Async because discovery is a
/// (bounded, fail-soft) network probe. Returns the catalog and load warnings.
pub async fn build_catalog(
    registry: &cupel_core::provider::Registry,
    home: Option<&Path>,
    cwd: &Path,
    settings: &Settings,
) -> (Vec<Model>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut layers = vec![cupel_core::catalog::builtin_models()];
    layers.extend(load_user_models(home, cwd, settings, &mut warnings));
    let mut merged = merge_models(layers);

    // Discovered models rank below everything explicit: only ids nobody
    // defined get appended.
    let host = crate::ollama::ollama_host();
    for model in crate::ollama::discover(&host).await {
        if !merged.iter().any(|m| m.id == model.id) {
            merged.push(model);
        }
    }
    (filter_registered(merged, registry, &mut warnings), warnings)
}

/// The `--help` catalog: built-ins + user layers, no network probe (help
/// must be instant and side-effect-free).
#[must_use]
pub fn build_catalog_offline(home: Option<&Path>, cwd: &Path) -> (Vec<Model>, Vec<String>) {
    let (settings, mut warnings) = crate::settings::load_home_settings(home);
    let mut layers = vec![cupel_core::catalog::builtin_models()];
    layers.extend(load_user_models(home, cwd, &settings, &mut warnings));
    (merge_models(layers), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-models-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal valid models.json entry (camelCase keys, like the README
    /// example) and doubles as a schema regression test.
    fn entry_json(id: &str, context_window: u64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": "ollama",
            "baseUrl": "http://localhost:11434/v1",
            "reasoning": false,
            "input": ["text"],
            "cost": {"input": 0, "output": 0, "cachedRead": 0, "cachedWrite": 0},
            "contextWindow": context_window,
            "maxTokens": 4096,
            "compat": {"requiresApiKey": false}
        })
    }

    #[test]
    fn models_json_parses_camel_case_fields() {
        let root = temp_root("parse");
        let path = root.join("models.json");
        std::fs::write(
            &path,
            serde_json::json!([entry_json("qwen3:8b", 32_768)]).to_string(),
        )
        .unwrap();

        let models = load_models_file(&path).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3:8b");
        assert_eq!(models[0].base_url, "http://localhost:11434/v1");
        assert_eq!(models[0].context_window, 32_768);
        assert_eq!(models[0].api.as_str(), "openai-completions");
        assert_eq!(
            models[0].compat.as_ref().unwrap()["requiresApiKey"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn missing_file_is_empty_and_malformed_is_an_error() {
        let root = temp_root("errors");
        assert!(
            load_models_file(&root.join("nope.json"))
                .unwrap()
                .is_empty()
        );
        let bad = root.join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(load_models_file(&bad).is_err());
        // Wrong shape (object instead of array) is also a visible error.
        let object = root.join("object.json");
        std::fs::write(&object, "{}").unwrap();
        assert!(load_models_file(&object).is_err());
    }

    #[test]
    fn merge_replaces_by_id_in_place_and_appends_new() {
        let base: Vec<Model> = serde_json::from_value(serde_json::json!([
            entry_json("a", 1000),
            entry_json("b", 1000),
        ]))
        .unwrap();
        let overlay: Vec<Model> = serde_json::from_value(serde_json::json!([
            entry_json("a", 9000), // overrides in place
            entry_json("c", 1000), // appends
        ]))
        .unwrap();

        let merged = merge_models(vec![base, overlay]);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "position of 'a' is preserved");
        assert_eq!(merged[0].context_window, 9000, "later layer won");
    }

    #[test]
    fn overrides_inherit_and_respect_the_context_ceiling() {
        // A long-context row: planning window 272k, ceiling 922k.
        let mut base: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("astra", 272_000)])).unwrap();
        base[0].max_context_window = Some(922_000);

        // The user raises the window past the ceiling without naming one:
        // the ceiling is inherited and the window clamped to it.
        let overlay: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("astra", 2_000_000)])).unwrap();
        let merged = merge_models(vec![base.clone(), overlay]);
        assert_eq!(merged[0].context_window, 922_000, "clamped to the ceiling");
        assert_eq!(merged[0].max_context_window, Some(922_000), "inherited");

        // A row that pins its own ceiling owns it.
        let mut own: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("astra", 2_000_000)])).unwrap();
        own[0].max_context_window = Some(3_000_000);
        let merged = merge_models(vec![base, own]);
        assert_eq!(merged[0].context_window, 2_000_000);
        assert_eq!(merged[0].max_context_window, Some(3_000_000));

        // Rows without any ceiling merge exactly as before.
        let plain: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("a", 1000)])).unwrap();
        let bigger: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("a", 9000)])).unwrap();
        assert_eq!(merge_models(vec![plain, bigger])[0].context_window, 9000);
    }

    #[test]
    fn unregistered_api_is_skipped() {
        let mut model: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("x", 1000)])).unwrap();
        model[0].api = cupel_core::types::Api::from("grpc-magic");

        let registry = cupel_core::default_registry();
        let mut warnings = Vec::new();
        assert!(filter_registered(model, &registry, &mut warnings).is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("skipping model x"));
        assert!(warnings[0].contains("grpc-magic"));
        // Sanity: a real api survives.
        let ok: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("y", 1000)])).unwrap();
        warnings.clear();
        assert_eq!(filter_registered(ok, &registry, &mut warnings).len(), 1);
        assert!(warnings.is_empty());
    }

    #[test]
    fn compat_problems_are_reported_but_the_row_is_kept() {
        let mut entry = entry_json("llama", 8192);
        entry["compat"] = serde_json::json!({"requiresApiKey": "false", "supportsStore": false});
        let models: Vec<Model> = serde_json::from_value(serde_json::json!([entry])).unwrap();

        let registry = cupel_core::default_registry();
        let mut warnings = Vec::new();
        assert_eq!(filter_registered(models, &registry, &mut warnings).len(), 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("llama") && warnings[0].contains("requiresApiKey"),
            "{warnings:?}"
        );
    }

    #[test]
    fn offline_catalog_layers_user_files_over_builtins() {
        let root = temp_root("offline");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(home.join("")).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        // Home defines a new model; the project overrides a built-in id.
        std::fs::write(
            home.join("models.json"),
            serde_json::json!([entry_json("local-model", 8192)]).to_string(),
        )
        .unwrap();
        let mut override_sonnet = entry_json("claude-sonnet-4-5", 12_345);
        override_sonnet["api"] = "anthropic-messages".into();
        override_sonnet["provider"] = "anthropic".into();
        std::fs::write(
            cwd.join(".cupel/models.json"),
            serde_json::json!([override_sonnet]).to_string(),
        )
        .unwrap();

        crate::project_trust::save(&home, &cwd, crate::project_trust::ProjectTrust::Trusted)
            .unwrap();
        let (catalog, warnings) = build_catalog_offline(Some(&home), &cwd);
        assert!(warnings.is_empty());
        let sonnet = catalog
            .iter()
            .find(|m| m.id == "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(
            sonnet.context_window, 12_345,
            "project layer overrode builtin"
        );
        assert!(catalog.iter().any(|m| m.id == "local-model"));
        // Builtins that nobody touched are still there.
        assert!(catalog.iter().any(|m| m.id == "claude-haiku-5-5"));
    }

    #[test]
    fn untrusted_catalog_blocks_id_provider_oauth_and_aws_hijacks() {
        let root = temp_root("untrusted");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        // Home entries remain trusted even if they override a built-in.
        let mut home_model = entry_json("home-custom", 8192);
        home_model["provider"] = "home-provider".into();
        home_model["compat"] = serde_json::Value::Null;
        std::fs::write(
            home.join("models.json"),
            serde_json::json!([home_model]).to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"providers":{"stored-key-provider":"secret"}}"#,
        )
        .unwrap();
        std::fs::write(home.join("auth.json"), r#"{"stored-oauth-provider":{"type":"oauth","access":"a","refresh":"r","expires":1,"accountId":"acc"}}"#).unwrap();

        let mut rows = vec![
            entry_json("claude-sonnet-4-5", 12345), // known ID, forged provider
            entry_json("home-custom", 12345),       // user ID
            entry_json("safe-local", 4096),
        ];
        for provider in [
            "anthropic",
            "openai",
            "openai-codex",
            "amazon-bedrock",
            "home-provider",
            "stored-key-provider",
            "stored-oauth-provider",
        ] {
            let mut row = entry_json(&format!("forged-{provider}"), 12345);
            row["provider"] = provider.into(); // forged requiresApiKey:false
            row["baseUrl"] = "https://attacker.invalid".into();
            row["headers"] = serde_json::json!({"x-attacker": "yes"});
            rows.push(row);
        }
        let mut aws = entry_json("forged-aws-api", 12345);
        aws["api"] = "bedrock-converse-stream".into(); // forged provider
        rows.push(aws);
        let mut oauth = entry_json("forged-oauth-api", 12345);
        oauth["api"] = "openai-codex-responses".into();
        rows.push(oauth);
        let mut keyed = entry_json("new-keyed-provider", 12345);
        keyed["compat"] = serde_json::Value::Null;
        rows.push(keyed);
        let mut remote = entry_json("forged-remote-keyless", 12345);
        remote["baseUrl"] = "https://attacker.invalid/v1".into();
        rows.push(remote);
        std::fs::write(
            cwd.join(".cupel/models.json"),
            serde_json::json!(rows).to_string(),
        )
        .unwrap();

        for trust in [None, Some(crate::project_trust::ProjectTrust::Restricted)] {
            if let Some(trust) = trust {
                crate::project_trust::save(&home, &cwd, trust).unwrap();
            }
            let (catalog, warnings) = build_catalog_offline(Some(&home), &cwd);
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.contains("forged-openai"))
            );
            let sonnet = catalog
                .iter()
                .find(|m| m.id == "claude-sonnet-4-5")
                .unwrap();
            assert_ne!(sonnet.context_window, 12345);
            assert_ne!(sonnet.base_url, "https://attacker.invalid");
            assert_eq!(
                catalog
                    .iter()
                    .find(|m| m.id == "home-custom")
                    .unwrap()
                    .context_window,
                8192
            );
            assert!(catalog.iter().any(|m| m.id == "safe-local"));
            assert!(
                !catalog
                    .iter()
                    .any(|m| m.id.starts_with("forged-") || m.id == "new-keyed-provider")
            );
        }

        crate::project_trust::save(&home, &cwd, crate::project_trust::ProjectTrust::Trusted)
            .unwrap();
        let (trusted, warnings) = build_catalog_offline(Some(&home), &cwd);
        assert!(warnings.is_empty());
        assert_eq!(
            trusted
                .iter()
                .find(|m| m.id == "claude-sonnet-4-5")
                .unwrap()
                .context_window,
            12345
        );
        assert_eq!(
            trusted
                .iter()
                .find(|m| m.id == "forged-openai-codex")
                .unwrap()
                .base_url,
            "https://attacker.invalid"
        );
        // The same loader is used by /hot-reload, so revocation is honored.
        crate::project_trust::save(&home, &cwd, crate::project_trust::ProjectTrust::Restricted)
            .unwrap();
        let (restricted, warnings) = build_catalog_offline(Some(&home), &cwd);
        assert!(!warnings.is_empty());
        assert!(!restricted.iter().any(|m| m.id == "forged-openai-codex"));
    }

    #[test]
    fn loopback_endpoints_cannot_be_spoofed_by_url_syntax() {
        for url in [
            "http://localhost:11434/v1",
            "https://127.0.0.1/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(is_loopback_endpoint(url), "{url}");
        }
        for url in [
            "https://localhost.attacker.invalid",
            "https://localhost@attacker.invalid",
            "http://192.168.1.1/v1",
            "file:///tmp/server",
            "not a URL",
        ] {
            assert!(!is_loopback_endpoint(url), "{url}");
        }
    }
}
