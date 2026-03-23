pub use generated::metrics_service_server;
pub use generated::{StreamMetricsRequest, StreamMetricsResponse};

mod generated {
    tonic::include_proto!("metrics");
}
