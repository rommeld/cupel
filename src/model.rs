#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Fireworks,
    OpenAI,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiProtocol {
    Anthropic,
    OpenAiResponse,
    OpenAiChatCompletions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Tools,
    Streaming,
    Reasoning,
    Vision,
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub model_id: String,
    pub model_display_name: String,
    pub model_provider: Provider,
    pub model_protocol: ApiProtocol,
    pub model_base_url: String,
    pub model_context_window: Option<u32>,
    pub model_max_output_token: Option<u32>,
    pub model_support: Vec<Support>,
    pub model_pricing: Option<Pricing>,
}

#[derive(Debug, Clone)]
pub struct Pricing {
    pub pricing_input_per_million: f64,
    pub pricing_output_per_million: f64,
    pub pricing_cache_read_per_million: Option<f64>,
    pub pricing_cache_write_per_million: Option<f64>,
}
