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
//! 3. `<cwd>/.cupel/models.json`,
//! 4. ollama auto-discovery (lowest: an explicit entry always beats a
//!    discovered one - that is the user's override channel for context
//!    window, reasoning, and compat flags).
//!
//! The catalog is resolved ONCE at startup in `main::run()` (discovery is a
//! network call; the TUI's key handlers are synchronous) and threaded to
//! the frontends via `SessionMeta.models`.

use std::path::Path;

use cupel_core::types::Model;

/// Parse one `models.json`: a JSON array of Model descriptors in the
/// workspace-wide camelCase serde form (`baseUrl`, `contextWindow`,
/// `maxTokens`, ...). A missing file is simply an empty layer; a MALFORMED
/// file is an error the caller must surface - unlike optional context
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
/// Parse errors are announced on stderr (visible in scrollback before the
/// TUI takes the screen - the same idiom as the "logging to ..." line) and
/// the broken layer is skipped, never aborting startup.
#[must_use]
pub fn load_user_models(home: Option<&Path>, cwd: &Path) -> Vec<Vec<Model>> {
    let mut layers = Vec::new();
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join("models.json"));
    }
    paths.push(cwd.join(".cupel/models.json"));

    for path in paths {
        match load_models_file(&path) {
            Ok(models) => layers.push(models),
            Err(e) => eprintln!("warning: ignoring models file: {e}"),
        }
    }
    layers
}

/// Merge catalog layers. An id collision REPLACES the earlier entry in
/// place (keeping its position, so each provider's first model - its
/// `/provider` default - stays stable); new ids append. Within one layer
/// the same rule applies, so a duplicated id in a single file is
/// last-wins.
#[must_use]
pub fn merge_models(layers: Vec<Vec<Model>>) -> Vec<Model> {
    let mut merged: Vec<Model> = Vec::new();
    for layer in layers {
        for model in layer {
            match merged.iter_mut().find(|m| m.id == model.id) {
                Some(existing) => *existing = model,
                None => merged.push(model),
            }
        }
    }
    merged
}

/// Drop entries whose `api` has no registered provider implementation -
/// they would only fail at request time. Guards the same invariant the
/// built-in catalog tests enforce ("every model has a registered
/// provider"), extended to user input: warn and skip, never abort.
#[must_use]
pub fn filter_registered(
    models: Vec<Model>,
    registry: &cupel_core::provider::Registry,
) -> Vec<Model> {
    models
        .into_iter()
        .filter(|model| {
            let registered = registry.get(model.api.as_str()).is_some();
            if !registered {
                tracing::warn!(
                    model = %model.id,
                    api = %model.api.as_str(),
                    "skipping model: no provider implements this api"
                );
                eprintln!(
                    "warning: skipping model {} - no provider implements api \"{}\"",
                    model.id,
                    model.api.as_str()
                );
            }
            registered
        })
        .collect()
}

/// The full startup catalog: built-ins, user layers, then ollama
/// discovery for ids not already defined. Async because discovery is a
/// (bounded, fail-soft) network probe.
pub async fn build_catalog(
    registry: &cupel_core::provider::Registry,
    home: Option<&Path>,
    cwd: &Path,
) -> Vec<Model> {
    let mut layers = vec![cupel_core::catalog::builtin_models()];
    layers.extend(load_user_models(home, cwd));
    let mut merged = merge_models(layers);

    // Discovered models rank BELOW everything explicit: only ids nobody
    // defined get appended.
    let host = crate::ollama::ollama_host();
    for model in crate::ollama::discover(&host).await {
        if !merged.iter().any(|m| m.id == model.id) {
            merged.push(model);
        }
    }
    filter_registered(merged, registry)
}

/// The `--help` catalog: built-ins + user layers, NO network probe (help
/// must be instant and side-effect-free).
#[must_use]
pub fn build_catalog_offline(home: Option<&Path>, cwd: &Path) -> Vec<Model> {
    let mut layers = vec![cupel_core::catalog::builtin_models()];
    layers.extend(load_user_models(home, cwd));
    merge_models(layers)
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
    /// example) - doubles as a schema regression test.
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
    fn unregistered_api_is_skipped() {
        let mut model: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("x", 1000)])).unwrap();
        model[0].api = cupel_core::types::Api::from("grpc-magic");

        let registry = cupel_core::default_registry();
        assert!(filter_registered(model, &registry).is_empty());
        // Sanity: a real api survives.
        let ok: Vec<Model> =
            serde_json::from_value(serde_json::json!([entry_json("y", 1000)])).unwrap();
        assert_eq!(filter_registered(ok, &registry).len(), 1);
    }

    #[test]
    fn offline_catalog_layers_user_files_over_builtins() {
        let root = temp_root("offline");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(home.join("")).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        // Home defines a new model; the project overrides a BUILTIN id.
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

        let catalog = build_catalog_offline(Some(&home), &cwd);
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
        assert!(catalog.iter().any(|m| m.id == "claude-haiku-4-5"));
    }
}
