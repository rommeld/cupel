#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_token: u64,
    pub output_token: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}
