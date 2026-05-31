#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelProvider {
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
    pub id: String,
    pub display_name: String,
    pub provider: ModelProvider,
    pub protocol: ApiProtocol,
    pub base_url: String,
    pub context_window: Option<u32>,
    pub max_output_token: Option<u32>,
    pub support: Vec<Support>,
    pub pricing: Option<ModelPricing>,
}

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: Option<f64>,
    pub cache_write_per_million: Option<f64>
}
