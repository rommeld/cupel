use cupel::db::DbPool;
use cupel::generated::cellar::{
    wine_bottle_service_server::WineBottleServiceServer,
    wine_cellar_service_server::WineCellarServiceServer,
};
use cupel::server::server_impl::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tonic::transport::Server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = "0.0.0.0:50051".parse()?;

    let db = DbPool::connect_in_memory()
        .await
        .expect("Failed to connect to database");

    let state = AppState::new(Arc::new(db));

    println!("Wine Cellar Server listening on {}", addr);

    Server::builder()
        .add_service(WineCellarServiceServer::new(state.clone()))
        .add_service(WineBottleServiceServer::new(state))
        .serve(addr)
        .await?;

    Ok(())
}
