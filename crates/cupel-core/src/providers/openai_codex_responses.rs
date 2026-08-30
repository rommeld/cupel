//! ChatGPT Codex backend provider - the `OpenAI` Responses dialect behind
//! a ChatGPT Plus/Pro subscription.
//!
//! The STREAM is the plain Responses SSE stream; what differs is
//! everything around it:
//!
//! - **URL**: `{base_url}/codex/responses` on `chatgpt.com/backend-api`,
//!   not `api.openai.com/v1/responses`.
//! - **Auth**: the bearer token is a ChatGPT OAuth ACCESS token (see
//!   `crate::oauth::openai_codex`), and the backend additionally demands
//!   the `chatgpt-account-id` header - extracted from that very token's
//!   JWT claim on every request.
//! - **Body**: `store: false` is mandatory (the backend rejects true),
//!   the system prompt travels in the `instructions` field instead of a
//!   leading message item, and there is no `max_output_tokens` - the
//!   backend manages the output budget itself.
//!
//! Deliberately NOT mirrored from pi: the WebSocket transport (pi's
//! default, with SSE as fallback - cupel speaks the fallback, which the
//! backend fully supports), zstd request compression, service tiers, and
//! the tool-search machinery. Each is an optimization on top of this
//! exact SSE path.

use serde_json::{Value, json};

use crate::{
    error::{InferenceError, Result},
    event_stream::{AssistantMessageStream, assistant_message_channel},
    model::clamp_thinking_level,
    oauth::openai_codex::{ORIGINATOR, account_id_from_access_token},
    provider::Provider,
    providers::{
        apply_custom_headers, error_message,
        openai_responses::{convert_items, normalize_id_part, process_response_stream, short_hash},
        with_cancel,
    },
    types::{
        Api, AssistantMessage, Context, Model, ModelThinkingLevel, StopReason, StreamOptions,
        ThinkingLevel,
    },
};

pub struct OpenAiCodexResponsesProvider {
    http: reqwest::Client,
}

impl OpenAiCodexResponsesProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiCodexResponsesProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for OpenAiCodexResponsesProvider {
    fn api(&self) -> &str {
        Api::OPENAI_CODEX_RESPONSES
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let (stream, sink) = assistant_message_channel();
        let model = model.clone();
        let http = self.http.clone();

        tokio::spawn(async move {
            if let Err(err) = run(&http, &model, &context, &options, &sink).await {
                let reason = if matches!(err, InferenceError::Aborted) {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                tracing::warn!(error = %err, "provider request failed");
                let msg = error_message(&model, reason, err.to_string());
                let _ = sink.error(reason, msg);
            }
        });
        stream
    }
}

#[tracing::instrument(name = "openai_codex_request", skip_all, fields(model = %model.id))]
async fn run(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &crate::event_stream::EventSink,
) -> Result<()> {
    let api_key = options
        .api_key
        .clone()
        .ok_or_else(|| InferenceError::MissingApiKey(model.provider.as_str().to_string()))?;
    // The backend routes by account; the id lives INSIDE the access
    // token, so a non-JWT "key" (someone pasted an sk-... API key) is
    // caught here with a pointer at the fix, not with the server 401.
    let account_id = account_id_from_access_token(&api_key).ok_or_else(|| {
        InferenceError::Other(
            "openai-codex needs a ChatGPT login, not an API key - run /login openai-codex"
                .to_string(),
        )
    })?;

    let body = build_request_body(model, context, options);
    // TRACE only: request bodies contain the user's code and prompts.
    tracing::trace!(body = %body, "request body");

    let mut req = http.post(resolve_codex_url(&model.base_url));
    for (name, value) in codex_headers(&api_key, &account_id, options.session_id.as_deref()) {
        req = req.header(name, value);
    }
    req = apply_custom_headers(req, model, options);
    if let Some(timeout) = options.timeout_ms {
        req = req.timeout(core::time::Duration::from_millis(timeout));
    }

    let response = with_cancel(options, req.json(&body).send()).await??;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(InferenceError::ApiStatus {
            status: status.as_u16(),
            body,
        });
    }

    // Same stream, same decoder as api.openai.com.
    process_response_stream(response, model, options, sink).await
}

/// `{base_url}/codex/responses`, tolerating a base that already carries
/// part of the path.
fn resolve_codex_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        return normalized.to_string();
    }
    if normalized.ends_with("/codex") {
        return format!("{normalized}/responses");
    }
    format!("{normalized}/codex/responses")
}

/// The headers the Codex backend requires, as data so tests can read
/// them. `session-id`/`x-client-request-id` give the backend cache
/// affinity.
fn codex_headers(
    api_key: &str,
    account_id: &str,
    session_id: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("authorization", format!("Bearer {api_key}")),
        ("chatgpt-account-id", account_id.to_string()),
        ("originator", ORIGINATOR.to_string()),
        ("user-agent", format!("cupel/{}", env!("CARGO_PKG_VERSION"))),
        ("openai-beta", "responses=experimental".to_string()),
        ("accept", "text/event-stream".to_string()),
        ("content-type", "application/json".to_string()),
    ];
    if let Some(session_id) = session_id {
        let clamped = clamp_cache_key(session_id);
        headers.push(("session-id", clamped.clone()));
        headers.push(("x-client-request-id", clamped));
    }
    headers
}

/// The API caps cache keys at 64 chars (same clamp as openai_responses).
fn clamp_cache_key(session_id: &str) -> String {
    session_id.chars().take(64).collect()
}

/// The model name the WIRE wants. Catalog ids are namespaced
/// ("codex/gpt-5.5") because cupel's catalog is one flat id namespace.
/// The openai provider already owns "gpt-5.6-sol" etc., and merge_models
/// replaces by id. The compat blob carries the backend's real name.
fn wire_model(model: &Model) -> String {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("requestModel"))
        .and_then(Value::as_str)
        .map_or_else(|| model.id.clone(), str::to_string)
}

fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    use crate::types::CacheRetention;

    let mut body = json!({
        "model": wire_model(model),
        // The backend REJECTS store:true ("Store must be set to false").
        // Stateless mode is not a choice.
        "store": false,
        "stream": true,
        // The system prompt is part of the `instructions`, never as an
        // input item.
        "instructions": context
            .system_prompt
            .clone()
            .unwrap_or_else(|| "You are a helpful assistant.".to_string()),
        "input": Value::Array(convert_items(model, context, normalize_tool_call_id_codex)),
        "text": {"verbosity": "low"},
        // Stateless mode ALWAYS replays encrypted reasoning.
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });

    let cache_retention = options.cache_retention.unwrap_or(CacheRetention::Short);
    if cache_retention != CacheRetention::None
        && let Some(session_id) = &options.session_id
    {
        body["prompt_cache_key"] = json!(clamp_cache_key(session_id));
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }

    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                        "strict": Value::Null,
                    })
                })
                .collect(),
        );
    }

    if model.reasoning {
        let requested = options.reasoning.map(|level| match level {
            ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
            ThinkingLevel::Low => ModelThinkingLevel::Low,
            ThinkingLevel::Medium => ModelThinkingLevel::Medium,
            ThinkingLevel::High => ModelThinkingLevel::High,
            ThinkingLevel::XHigh => ModelThinkingLevel::XHigh,
        });
        let clamped = requested.map(|level| clamp_thinking_level(model, level));
        if let Some(level) = clamped
            && level != ModelThinkingLevel::Off
        {
            let effort = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(level.as_str()).cloned().flatten())
                .unwrap_or_else(|| level.as_str().to_string());
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
        }
    }
    body
}

/// Codex's tool-call id normalization is same as openai_responses mechanics,
/// but ids minted by the plain `openai` provider count as FAMILY, not foreign.
fn normalize_tool_call_id_codex(id: &str, _model: &Model, source: &AssistantMessage) -> String {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return normalize_id_part(id);
    };
    let normalized_call_id = normalize_id_part(call_id);
    let is_foreign = !matches!(
        source.provider.as_str(),
        crate::types::Provider::OPENAI | crate::types::Provider::OPENAI_CODEX
    );
    let mut normalized_item_id = if is_foreign {
        format!("fc_{}", short_hash(item_id))
    } else {
        normalize_id_part(item_id)
    };
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantContent, InputModality, Message, ModelCost, Provider, TextContent,
        ThinkingLevelMap, Tool, Usage, UserContentBody, UserMessage, now_ms,
    };

    /// A Codex catalog row as M4 will generate it: reasoning on, the
    /// minimal->low rename pinned, ChatGPT backend base URL.
    fn codex_model() -> Model {
        let mut map = ThinkingLevelMap::new();
        map.insert("minimal".to_string(), Some("low".to_string()));
        Model {
            id: "gpt-5.4".to_string(),
            name: "GPT-5.4".to_string(),
            api: Api::from(Api::OPENAI_CODEX_RESPONSES),
            provider: Provider::from(Provider::OPENAI_CODEX),
            base_url: "https://chatgpt.com/backend-api".to_string(),
            reasoning: true,
            thinking_level_map: Some(map),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost::default(),
            context_window: 272_000,
            max_tokens: 128_000,
            headers: None,
            compat: None,
        }
    }

    fn context_with_prompt() -> Context {
        Context {
            system_prompt: Some("You are cupel.".to_string()),
            messages: vec![Message::User(UserMessage {
                content: UserContentBody::Text("hi".to_string()),
                timestamp: now_ms(),
            })],
            tools: None,
        }
    }

    #[test]
    fn codex_url_tolerates_partial_bases() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://proxy.example/codex/responses"),
            "https://proxy.example/codex/responses"
        );
    }

    #[test]
    fn body_moves_the_system_prompt_into_instructions() {
        let model = codex_model();
        let body = build_request_body(&model, &context_with_prompt(), &StreamOptions::default());
        assert_eq!(body["instructions"], json!("You are cupel."));
        // The input carries the user message but NO system/developer item.
        let input = body["input"].as_array().expect("input array");
        assert!(
            input
                .iter()
                .all(|item| item["role"] != json!("system") && item["role"] != json!("developer")),
            "system prompt must not appear as an item: {input:?}"
        );
        assert_eq!(input.len(), 1, "just the user message");

        // No system prompt at all: pi still sends the field, with its
        // fallback text.
        let body = build_request_body(
            &model,
            &Context {
                system_prompt: None,
                messages: Vec::new(),
                tools: None,
            },
            &StreamOptions::default(),
        );
        assert_eq!(body["instructions"], json!("You are a helpful assistant."));
    }

    #[test]
    fn body_pins_the_codex_invariants() {
        let model = codex_model();
        let options = StreamOptions {
            max_tokens: Some(4096),
            session_id: Some("session-abc".to_string()),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &context_with_prompt(), &options);
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["text"], json!({"verbosity": "low"}));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tool_choice"], json!("auto"));
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["prompt_cache_key"], json!("session-abc"));
        // max_tokens was SET in the options - and must not be sent.
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn reasoning_maps_levels_and_omits_off() {
        let model = codex_model();
        let context = context_with_prompt();

        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &context, &options);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "medium", "summary": "auto"})
        );

        // The catalog pins minimal -> "low" (the backend has no minimal).
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Minimal),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &context, &options);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );

        // Off omits the parameter entirely - never effort "none" here.
        let body = build_request_body(&model, &context, &StreamOptions::default());
        assert!(body.get("reasoning").is_none());
        // Encrypted reasoning stays included even with thinking off.
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn tools_carry_an_explicit_null_strict() {
        let model = codex_model();
        let mut context = context_with_prompt();
        context.tools = Some(vec![Tool {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({"type": "object"}),
        }]);
        let body = build_request_body(&model, &context, &StreamOptions::default());
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert_eq!(tool["name"], json!("read"));
        // Present AND null - get() distinguishes that from absent.
        assert!(tool.get("strict").is_some_and(Value::is_null));
    }

    #[test]
    fn headers_carry_identity_and_session_affinity() {
        let headers = codex_headers("token-1", "acc-1", Some("session-xyz"));
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("authorization"), Some("Bearer token-1"));
        assert_eq!(get("chatgpt-account-id"), Some("acc-1"));
        assert_eq!(get("originator"), Some("cupel"));
        assert_eq!(get("openai-beta"), Some("responses=experimental"));
        assert_eq!(get("accept"), Some("text/event-stream"));
        assert_eq!(get("session-id"), Some("session-xyz"));
        assert_eq!(get("x-client-request-id"), Some("session-xyz"));
        assert!(get("user-agent").is_some_and(|ua| ua.starts_with("cupel/")));
        // No session id, no affinity headers.
        assert!(
            codex_headers("t", "a", None)
                .iter()
                .all(|(key, _)| *key != "session-id")
        );
    }

    #[test]
    fn tool_call_ids_from_the_openai_family_stay_usable() {
        let model = codex_model();
        let assistant_from = |provider: &str| AssistantMessage {
            content: vec![AssistantContent::Text(TextContent::plain(""))],
            api: Api::from(Api::OPENAI_RESPONSES),
            provider: Provider::from(provider),
            model: "gpt-5.4".to_string(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };

        // openai and openai-codex ids pass through by name...
        for family in ["openai", "openai-codex"] {
            assert_eq!(
                normalize_tool_call_id_codex("call_1|fc_abc", &model, &assistant_from(family)),
                "call_1|fc_abc"
            );
        }
        // ...anthropic-minted ids are re-derived as fc_<hash>.
        let foreign =
            normalize_tool_call_id_codex("toolu_1|toolu_1", &model, &assistant_from("anthropic"));
        let (call_id, item_id) = foreign.split_once('|').expect("two halves");
        assert_eq!(call_id, "toolu_1");
        assert!(item_id.starts_with("fc_"), "{item_id}");
        assert_ne!(item_id, "fc_toolu_1", "hash-derived, not renamed");
    }

    #[test]
    fn the_wire_model_comes_from_the_compat_blob() {
        let mut model = codex_model();
        // Without the knob the id goes out verbatim (user models.json).
        let body = build_request_body(&model, &context_with_prompt(), &StreamOptions::default());
        assert_eq!(body["model"], json!("gpt-5.4"));
        // Catalog rows carry codex/-namespaced ids plus the real name.
        model.id = "codex/gpt-5.4".to_string();
        model.compat = Some(json!({"requestModel": "gpt-5.4"}));
        let body = build_request_body(&model, &context_with_prompt(), &StreamOptions::default());
        assert_eq!(body["model"], json!("gpt-5.4"));
    }

    #[test]
    fn codex_api_is_registered_in_the_default_registry() {
        assert!(
            crate::default_registry()
                .get(Api::OPENAI_CODEX_RESPONSES)
                .is_some(),
            "lib.rs must register the codex provider"
        );
    }
}
