pub use generated::metrics_service_server;
pub use generated::{MetricSnapshot, StreamRequest};

mod generated {
    tonic::include_proto!("metrics");
}
