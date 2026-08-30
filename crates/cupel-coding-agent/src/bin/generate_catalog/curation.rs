//! The curated built-in catalog - WHICH models cupel ships, plus the
//! cupel-side knowledge models.dev does not carry: base URLs (missing
//! upstream for anthropic/openai), the API family per model, compat
//! quirks, and thinking-map exceptions.
//!
//! Routine maintenance happens HERE: adding a model = one row in
//! [`PROVIDERS`], then `cargo run -p cupel-coding-agent --bin generate-cataglo`.

use cupel_core::types::{Api, Provider};

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Empty on purpose: the AWS SDK derives the endpoint from the region
/// (see providers::bedrock::build_client).
pub const BEDROCK_BASE_URL: &str = "";
/// Fireworks' Anthropic-compatible endpoint (most of their models).
pub const FIREWORKS_ANTHROPIC_BASE_URL: &str = "https://api.fireworks.ai/inference";
/// Fireworks' Anthropic-compatible endpoint (most of ther models).
pub const FIREWORKS_COMPLETIONS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
/// OpenRouter's OpenAI completions compatible endpoint
pub const OPENROUTER_COMPLETIONS_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// The ChatGPT Codex backend (subscription auth, see cupel-core's
/// oauth::openai_codex + providers::openai_codex_responses).
pub const OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// How a curated model's thinkingLevelMap is produced.
pub enum Thinking {
    /// Budget-based thinking (Anthropic budget path and the Fireworks
    /// anthropic-compat endpoint): no map, every cupel level allowed.
    /// models.dev's effort options describe those vendors' NATIVE APIs,
    /// not the anthropic-compat endpoint cupel drives - deriving a map
    /// here would wrongly disable levels.
    Budget,
    /// Derive from models.dev effort values (models_dev.rs).
    FromEffort,
    /// Pin an explicit map for scales cupel must remap (GLM 5.2).
    Explicit(&'static [(&'static str, Option<&'static str>)]),
}

/// Named compat templates - the per-API quirk blobs from the old
/// hand-written catalog, now defined in exactly one place.
pub enum Compat {
    None,
    FireworksAnthropic,
    FireworksCompletions,
    AdaptiveAnthropic,
    OpenrouterCompletions,
}

impl Compat {
    pub fn to_value(&self) -> Option<serde_json::Value> {
        match self {
            Self::None => None,
            Self::FireworksAnthropic => Some(serde_json::json!({
                "sendSessionAffinityHeaders": true,
                "supportsEagerToolInputStreaming": false,
                "supportsCacheControlOnTools": false,
                "supportsLongCacheRetention": false,
            })),
            Self::FireworksCompletions => Some(serde_json::json!({
                "supportsStore": false,
                "supportsDeveloperRole": false,
            })),
            Self::AdaptiveAnthropic => Some(serde_json::json!({
                "forceAdaptiveThinking": true,
                "supportsTemperature": false,
            })),
            Self::OpenrouterCompletions => Some(serde_json::json!({
                "thinkingFormat": "openrouter",
                "supportsDeveloperRole": false,
            })),
        }
    }
}

/// One curated model: the models.dev id (== cupel id) plus cupel-side facts.
pub struct Curated {
    pub id: &'static str,
    /// None = take models.dev's display name verbatim.
    pub rename: Option<&'static str>,
    pub api: &'static str,
    pub base_url: &'static str,
    pub thinking: Thinking,
    pub compat: Compat,
}

pub struct CuratedProvider {
    /// Key in models.dev's api.json ("fireworks-ai", ...).
    pub models_dev_id: &'static str,
    /// cupel's provider id ("fireworks", ...).
    pub cupel_id: &'static str,
    pub models: &'static [Curated],
}

/// GLM 5.2 on Fireworks: cupel levels remapped onto Fireworks' effort
/// scale - off maps to none, minimal is unsupported, low /medium collapse
/// to high. The xhigh entry is dead under cupel's key-absence rule
/// (model.rs) but kept verbatim from the old catalog.
const GLM52_THINKING: &[(&str, Option<&str>)] = &[
    ("off", Some("none")),
    ("minimal", None),
    ("low", Some("high")),
    ("medium", Some("high")),
    ("xhigh", Some("max")),
];

/// Kimi K2.7 Code on OpenRouter is always-thinking.
const KIMI_K27_CODE_OPENROUTER_THINKING: &[(&str, Option<&str>)] = &[("off", None)];

// Compact row constructors, one per model family - the same shape the
// old catalog.rs used (fireworks_anthropic / fireworks_glm52 helpers).
const fn anthropic(id: &'static str, rename: Option<&'static str>) -> Curated {
    Curated {
        id,
        rename,
        api: Api::ANTHROPIC_MESSAGES,
        base_url: ANTHROPIC_BASE_URL,
        thinking: Thinking::Budget,
        compat: Compat::None,
    }
}

const fn openai(id: &'static str, rename: Option<&'static str>) -> Curated {
    Curated {
        id,
        rename,
        api: Api::OPENAI_RESPONSES,
        base_url: OPENAI_BASE_URL,
        thinking: Thinking::FromEffort,
        compat: Compat::None,
    }
}

const fn bedrock(id: &'static str, rename: Option<&'static str>, thinking: Thinking) -> Curated {
    Curated {
        id,
        rename,
        api: Api::BEDROCK_CONVERSE_STREAM,
        base_url: BEDROCK_BASE_URL,
        thinking,
        compat: Compat::None,
    }
}

const fn fireworks_anthropic(id: &'static str) -> Curated {
    Curated {
        id,
        rename: None,
        api: Api::ANTHROPIC_MESSAGES,
        base_url: FIREWORKS_ANTHROPIC_BASE_URL,
        thinking: Thinking::Budget,
        compat: Compat::FireworksAnthropic,
    }
}

const fn fireworks_glm(id: &'static str) -> Curated {
    Curated {
        id,
        rename: None,
        api: Api::OPENAI_COMPLETIONS,
        base_url: FIREWORKS_COMPLETIONS_BASE_URL,
        thinking: Thinking::Explicit(GLM52_THINKING),
        compat: Compat::FireworksCompletions,
    }
}

const fn openrouter(id: &'static str, thinking: Thinking) -> Curated {
    Curated {
        id,
        rename: None,
        api: Api::OPENAI_COMPLETIONS,
        base_url: OPENROUTER_COMPLETIONS_BASE_URL,
        thinking,
        compat: Compat::OpenrouterCompletions,
    }
}

/// Table order = catalog order = /model and /provider order.
///
/// claude-sonnet-5 MUST stay the very first row: test fixtures acroos
/// the workspace use builtin_models().remove(0), and catalog_providers'
/// per-provder default is "first in catalog order". The generator's
/// validate() enforces this.
pub const PROVIDERS: &[CuratedProvider] = &[
    CuratedProvider {
        models_dev_id: "anthropic",
        cupel_id: Provider::ANTHROPIC,
        models: &[
            anthropic("claude-sonnet-5", None),
            anthropic("claude-opus-5", None),
            Curated {
                id: "claude-fable-5",
                rename: None,
                api: Api::ANTHROPIC_MESSAGES,
                base_url: ANTHROPIC_BASE_URL,
                // Fable 5 is adaptive-only: effort levels instead of
                // token budgets, and no temperature parameter.
                thinking: Thinking::FromEffort,
                compat: Compat::AdaptiveAnthropic,
            },
            anthropic("claude-haiku-4-5", Some("Claude Haiku 4.5")),
            anthropic("claude-sonnet-4-6", None),
            anthropic("claude-sonnet-4-5", Some("Claude Sonnet 4.5")),
        ],
    },
    CuratedProvider {
        models_dev_id: "openai",
        cupel_id: Provider::OPENAI,
        models: &[
            openai("gpt-5.6-sol", Some("GPT-5.6 Solar")),
            openai("gpt-5.6-luna", None),
            openai("gpt-5.6-terra", None),
        ],
    },
    CuratedProvider {
        models_dev_id: "amazon-bedrock",
        cupel_id: Provider::AMAZON_BEDROCK,
        models: &[
            bedrock(
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                Some("Claude Sonnet 4.5 (Bedrock)"),
                Thinking::Budget,
            ),
            bedrock(
                "us.anthropic.claude-sonnet-5",
                Some("Claude Sonnet 5 (Bedrock)"),
                Thinking::FromEffort,
            ),
            bedrock(
                "us.anthropic.claude-fable-5",
                Some("Claude Fable 5 (Bedrock)"),
                Thinking::FromEffort,
            ),
        ],
    },
    CuratedProvider {
        models_dev_id: "fireworks-ai",
        cupel_id: Provider::FIREWORKS,
        models: &[
            fireworks_anthropic("accounts/fireworks/models/kimi-k2p7-code"),
            fireworks_anthropic("accounts/fireworks/models/deepseek-v4-flash-0731"),
            fireworks_anthropic("accounts/fireworks/models/deepseek-v4-pro-0813"),
            fireworks_anthropic("accounts/fireworks/models/kimi-k2p6"),
            fireworks_anthropic("accounts/fireworks/models/minimax-m3"),
            fireworks_anthropic("accounts/fireworks/models/qwen3p7-plus"),
            fireworks_anthropic("accounts/fireworks/models/kimi-k3"),
            fireworks_anthropic("accounts/fireworks/routers/kimi-k3-fast"),
            fireworks_glm("accounts/fireworks/models/glm-5p2"),
            fireworks_glm("accounts/fireworks/routers/glm-5p2-fast"),
        ],
    },
    CuratedProvider {
        models_dev_id: "openrouter",
        cupel_id: Provider::OPENROUTER,
        models: &[
            openrouter("qwen/qwen3.8-max", Thinking::FromEffort),
            openrouter(
                "moonshotai/kimi-k2.7-code",
                Thinking::Explicit(KIMI_K27_CODE_OPENROUTER_THINKING),
            ),
            openrouter("z-ai/glm-5.3", Thinking::FromEffort),
            openrouter("deepseek/deepseek-v4-pro", Thinking::FromEffort),
            openrouter("x-ai/grok-4.6", Thinking::FromEffort),
            openrouter("google/gemini-3.7-flash", Thinking::FromEffort),
        ],
    },
];

// ---------------------------------------------------------------------------
// OpenAI Codex (ChatGPT subscription) - pinned rows, not models.dev rows
// ---------------------------------------------------------------------------

/// One Codex model, pinned by hand. models.dev has no `openai-codex`
/// provider (subscription backends carry no public price sheet), so pi
/// keeps an explicit list in generate-models.ts ("we keep a small,
/// explicit list to avoid aliases") - this table is that list, verbatim:
/// same ids, names, prices, and limits.
///
/// The `id` is the BACKEND's model name; the generator namespaces the
/// catalog id as `codex/<id>` and stores the backend name in compat's
/// `requestModel`. That split exists because cupel's catalog is one flat
/// id namespace (merge_models replaces by id, /model addresses by id) -
/// and the openai provider already owns "gpt-5.6-sol" etc.
pub struct PinnedCodex {
    pub id: &'static str,
    pub name: &'static str,
    /// Codex Spark is text-only; everything else takes images too.
    pub vision: bool,
    /// $/M: input, output, cache read, cache write.
    pub cost: (f64, f64, f64, f64),
    /// Whether the >272k long-context tier applies (input x2, output
    /// x1.5, cache x2 - pi's withOpenAiLongContextPricing).
    pub long_context_tier: bool,
    pub context_window: u64,
}

/// Row order = catalog order: the first row is the `/provider
/// openai-codex` default. gpt-5.6-sol leads to match the openai
/// provider's curation (same family, same default instinct); pi lists
/// alphabetically, which would make the light Spark model the default.
pub const OPENAI_CODEX_MODELS: &[PinnedCodex] = &[
    PinnedCodex {
        id: "gpt-5.6-sol",
        name: "GPT-5.6 Sol",
        vision: true,
        cost: (5.0, 30.0, 0.5, 6.25),
        long_context_tier: true,
        context_window: 272_000,
    },
    PinnedCodex {
        id: "gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        vision: true,
        cost: (0.2, 1.2, 0.02, 0.25),
        long_context_tier: true,
        context_window: 272_000,
    },
    PinnedCodex {
        id: "gpt-5.6-terra",
        name: "GPT-5.6 Terra",
        vision: true,
        cost: (2.0, 12.0, 0.2, 2.5),
        long_context_tier: true,
        context_window: 272_000,
    },
    PinnedCodex {
        id: "gpt-5.5",
        name: "GPT-5.5",
        vision: true,
        cost: (5.0, 30.0, 0.5, 0.0),
        long_context_tier: true,
        context_window: 272_000,
    },
    PinnedCodex {
        id: "gpt-5.4",
        name: "GPT-5.4",
        vision: true,
        cost: (2.5, 15.0, 0.25, 0.0),
        long_context_tier: true,
        context_window: 272_000,
    },
    PinnedCodex {
        id: "gpt-5.4-mini",
        name: "GPT-5.4 mini",
        vision: true,
        cost: (0.75, 4.5, 0.075, 0.0),
        long_context_tier: false,
        context_window: 272_000,
    },
    PinnedCodex {
        id: "gpt-5.3-codex-spark",
        name: "GPT-5.3 Codex Spark",
        vision: false,
        cost: (1.75, 14.0, 0.175, 0.0),
        long_context_tier: false,
        context_window: 128_000,
    },
];
