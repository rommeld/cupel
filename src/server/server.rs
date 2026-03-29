use crate::db::DbPool;
use crate::generated::cellar;

pub struct WineCellarServiceImpl {
    db: DbPool,
}

#[tonic::async_trait]
impl WineCellarService for WineCellarServiceImpl {
    async fn create_wine_bottle(...) {
        todo!("CRUD implemenation.")
    }
}
