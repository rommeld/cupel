//! A minimal serde mirror of the models.dev catalog - only what the
//! generator reads. models.dev ships ~60 providers in fluctuating
//! shapes, so parsing happens in two stages: the outer file is read as
//! generic JSON, and only the providers named in curation.rs are
//! typed-parsed. A malformed entry in some unrelated provider can then
//! never break catalog generation.

use std::collections::{BTreeMap, BTreeSet};

use cupel_core::types::ThinkingLevelMap;
use serde::Deserialize;

/// One provider block. Each model stays raw JSON until it is actually
/// curated - only curated entries must parse as [`ModelEntry`].
#[derive(Debug, Deserialize)]
pub struct ProviderEntry {
    pub models: BTreeMap<String, serde_json::Value>,
}

/// The per-model subset the generator maps into `cupel_core::types::Model`.
/// Every field is defaulted so sparse upstream entries still parse; hard
/// requirements (cost present, usable limits) are enforced later, where
/// the error message can name the curated model.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ModelEntry {
    pub name: String,
    pub reasoning: bool,
    pub reasoning_options: Vec<ReasoningOption>,
    pub modalities: Modalities,
    /// JSON `null` on image/embedding models - hence Option.
    pub cost: Option<Cost>,
    pub limit: Limit,
}

/// How a model's thinking is switched upstream. Internally tagged on
/// "type"; struct variants tolerate extra fields, and `Unknown` swallows
/// any tag models.dev invents later.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Toggle,
    Effort {
        /// models.dev allows JSON null inside the list.
        values: Vec<Option<String>>,
    },
    BudgetTokens,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Modalities {
    pub input: Vec<String>,
}

/// USD per million tokes, models.dev field names.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// Absent for providers that do not bill cache writes (Fireworks).
    pub cache_write: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Limit {
    pub context: u64,
    pub output: u64,
}

/// Stage 1 + 2: parse the outer object generically, then typed-parse
/// only the `wanted` provider blocks.
pub fn parse_wanted(raw: &str, wanted: &[&str]) -> Result<BTreeMap<String, ProviderEntry>, String> {
    let root: serde_json::Value =
        serde_json::from_str(raw).map_err(|error| format!("models.dev JSON: {error}"))?;
    let providers = root.as_object().ok_or_else(|| {
        "models.dev JSON: top level is not an
            object"
            .to_string()
    })?;
    let mut out = BTreeMap::new();
    for &id in wanted {
        let value = providers.get(id).ok_or_else(|| {
            format!(
                "models.dev no longer
                lists provider {id:?}"
            )
        })?;
        let entry: ProviderEntry = serde_json::from_value(value.clone())
            .map_err(|error| format!("models.dev provider {id:?}: {error}"))?;
        out.insert(id.to_string(), entry);
    }
    Ok(out)
}

impl ProviderEntry {
    /// Typed view of one curated model; failures names the exact entry.
    pub fn model(&self, provider_id: &str, model_id: &str) -> Result<ModelEntry, String> {
        let value = self.models.get(model_id).ok_or_else(|| {
            format!(
                "models.dev no longer lists {provider_id}/
                {model_id} - update curation.rs"
            )
        })?;
        serde_json::from_value(value.clone()).map_err(|error| {
            format!(
                "models.dev entry
                {provider_id}/{model_id}: {error}"
            )
        })
    }
}

/// Derive cupel's thinkingLevelMap from models.dev effort values.
///
/// - an entry 'level -> null` disables that level,
/// - a SUPPORTED level needs NO entry (the provider's identity fallback
/// sends the level's own name),
/// xhigh is special-cased by supported_thinking_levels: it is
/// selectable onyl while its key is ABSENT - even `xhigh -> "xhigh"`
/// would disable it. Supported xhigh therefore means: omit the key.
pub fn thinking_level_map_from_effort(options: &[ReasoningOption]) -> Option<ThinkingLevelMap> {
    let mut effort: Vec<String> = Vec::new();
    let mut has_toggle = false;
    for option in options {
        match option {
            ReasoningOption::Effort { values } => {
                // flatten() drops the JSON nulls models.dev allows here.
                effort.extend(values.iter().flatten().cloned());
            }
            ReasoningOption::Toggle => has_toggle = true,
            _ => {}
        }
    }
    // No effort scale at all (budget/toggle-only models): no map -
    // every cupel level stays selectable, the provider maps levels to
    // token budgets.
    if effort.is_empty() {
        return None;
    }
    let supported: BTreeSet<&str> = effort.iter().map(String::as_str).collect();
    // Effort values without any cupel equivalent (e.g. only "default"):
    // treat as if models.dev had said nothing, rather than disabling
    // every level.
    let known = ["none", "minimal", "low", "medium", "high", "xhigh"];
    if !known.iter().any(|level| supported.contains(level)) {
        return None;
    }

    let mut map = ThinkingLevelMap::new();
    // "off": an explicit "none" effort value is sent as effort "none",;
    // a toggle-capable model switches off natively (no entry needed); a
    // pure-effort model without "none" cannot be switched off (-> null).
    if supported.contains("none") {
        map.insert("off".to_string(), Some("none".to_string()));
    } else if !has_toggle {
        map.insert("off".to_string(), None);
    }
    for level in ["minimal", "low", "medium", "high"] {
        if !supported.contains(level) {
            map.insert(level.to_string(), None);
        }
    }
    if !supported.contains("xhigh") {
        map.insert("xhigh".to_string(), None);
    }
    // models.dev "max" has no cupel thinking level and is ignored.
    if map.is_empty() { None } else { Some(map) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "anthropic": {
            "id": "anthropic",
            "models": {
                "claude-sonnet-5": {
                    "name": "Claude Sonnet 5",
                    "reasoning": true,
                    "reasoning_options": [
                        {"type": "toggle"},
                        {"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]},
                        {"type": "hyperspace", "warp": 9}
                    ],
                    "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]},
                    "cost": {"input": 2, "output": 10, "cache_read": 0.2, "cache_write": 2.5},
                    "limit": {"context": 1000000, "output": 128000}
                },
                "paint-o-matic": {"name": "Painter", "cost": null, "modalities": {"input": ["image"]}}
            }
        },
        "weird-provider": {"models": "not-even-an-object"}
    }"#;

    #[test]
    fn parse_wanted_ignores_unrelated_broken_providers() {
        // "weird-provider" is malformed, but we never asked for it.
        let catalog = parse_wanted(FIXTURE, &["anthropic"]).expect("wanted providers parse");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog["anthropic"].models.len(), 2);
    }

    #[test]
    fn parse_wanted_fails_loudly_on_missing_or_broken_wanted_providers() {
        let missing = parse_wanted(FIXTURE, &["anthropic", "openai"]).unwrap_err();
        assert!(
            missing.contains("models.dev no longer\n                lists provider \"openai\""),
            "{missing}"
        );
        let broken = parse_wanted(FIXTURE, &["weird-provider"]).unwrap_err();
        assert!(broken.contains("weird-provider"), "{broken}");
    }

    #[test]
    fn unknown_reasoning_option_types_are_tolerated() {
        let catalog = parse_wanted(FIXTURE, &["anthropic"]).expect("fixture parses");
        let entry = catalog["anthropic"]
            .model("anthropic", "claude-sonnet-5")
            .expect("entry parses");
        assert_eq!(entry.reasoning_options.len(), 3);
        assert!(matches!(
            entry.reasoning_options[2],
            ReasoningOption::Unknown
        ));
        // The sparse image model parses too; only cost is missing.
        let painter = catalog["anthropic"]
            .model("anthropic", "paint-o-matic")
            .expect("sparse entry parses");
        assert!(painter.cost.is_none());
    }

    fn effort(values: &[&str]) -> ReasoningOption {
        ReasoningOption::Effort {
            values: values.iter().map(|v| Some((*v).to_string())).collect(),
        }
    }

    fn map_of(pairs: &[(&str, Option<&str>)]) -> ThinkingLevelMap {
        pairs
            .iter()
            .copied()
            .map(|(level, value)| (level.to_string(), value.map(str::to_string)))
            .collect()
    }

    #[test]
    fn gpt56_shaped_effort_keeps_xhigh_by_omission() {
        // The trap this derivation exists for: xhigh must NOT appear in
        // the map when it is supported (see model.rs XHigh arm).
        let options = [effort(&["none", "low", "medium", "high", "xhigh", "max"])];
        let map = thinking_level_map_from_effort(&options).expect("map derived");
        assert_eq!(map, map_of(&[("off", Some("none")), ("minimal", None)]));
    }

    #[test]
    fn fable_shaped_effort_disables_off_without_a_toggle() {
        let options = [effort(&["low", "medium", "high", "xhigh", "max"])];
        let map = thinking_level_map_from_effort(&options).expect("map derived");
        assert_eq!(map, map_of(&[("off", None), ("minimal", None)]));
    }

    #[test]
    fn budget_only_models_get_no_map() {
        assert!(thinking_level_map_from_effort(&[ReasoningOption::BudgetTokens {}]).is_none());
        assert!(thinking_level_map_from_effort(&[]).is_none());
    }

    #[test]
    fn toggle_plus_full_effort_needs_no_map_at_all() {
        // Everything supported, off handled by the toggle: empty map
        // collapses to None.
        let options = [
            ReasoningOption::Toggle {},
            effort(&["minimal", "low", "medium", "high", "xhigh"]),
        ];
        assert!(thinking_level_map_from_effort(&options).is_none());
    }

    #[test]
    fn unusable_effort_values_mean_no_map() {
        let options = [ReasoningOption::Effort {
            values: vec![None, Some("default".to_string())],
        }];
        assert!(thinking_level_map_from_effort(&options).is_none());
    }
}
