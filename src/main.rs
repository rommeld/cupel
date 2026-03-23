use cupel::metric_service::MetricsServiceImpl;
use cupel::proto::metrics_service_server::MetricsServiceServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::]:50051".parse()?;

    println!("MetricsService listening on {}", addr);
    println!("Test it with:");
    println!(
        r#"  grpcurl -plaintext -d '{{"interval_ms": 1000}}' localhost:50051 metrics.MetricsService/StreamMetrics"#
    );

    tonic::transport::Server::builder()
        .add_service(MetricsServiceServer::new(MetricsServiceImpl))
        .serve(addr)
        .await?;

    Ok(())
}
