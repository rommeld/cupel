use cupel::db::DbPool;
use cupel::generated::cellar::{
    CreateWineBottleRequest, CreateWineCellarRequest, DeleteWineBottleRequest,
    DeleteWineCellarRequest, GetWineBottleRequest, GetWineCellarRequest, ListWineBottleRequest,
    ListWineCellarRequest, PaginationParams, UpdateWineBottleRequest, UpdateWineCellarRequest,
    WineColor, wine_bottle_service_client::WineBottleServiceClient,
    wine_bottle_service_server::WineBottleServiceServer,
    wine_cellar_service_client::WineCellarServiceClient,
    wine_cellar_service_server::WineCellarServiceServer,
};
use cupel::server::server_impl::AppState;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::spawn;
use tonic::transport::Server;

async fn setup_test_server() -> String {
    let db = DbPool::connect_in_memory()
        .await
        .expect("Failed to connect to in-memory database");
    let state = AppState::new(Arc::new(db));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind to random port");
    let addr = listener.local_addr().expect("Failed to get local address");
    let addr_str = format!("http://{}", addr);

    spawn(async move {
        Server::builder()
            .add_service(WineCellarServiceServer::new(state.clone()))
            .add_service(WineBottleServiceServer::new(state))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("Server error");
    });

    addr_str
}

mod wine_bottle_tests {
    use super::*;

    #[tokio::test]
    async fn test_create_wine_bottle() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let request = CreateWineBottleRequest {
            name: Some("Château Margaux".to_string()),
            producer: Some("Château Margaux".to_string()),
            grape_variety: vec!["Cabernet Sauvignon".to_string()],
            vintage: Some(2015),
            country: Some("France".to_string()),
            region: Some("Bordeaux".to_string()),
            color: Some(WineColor::Red as i32),
            quantity: Some(6),
            purchase_date: Some("2019-01-15".to_string()),
            purchase_price: Some(499.99),
            currency_code: Some("USD".to_string()),
            drink_from_year: Some(2025),
            drink_to_year: Some(2060),
            notes: Some("Excellent vintage".to_string()),
            rating: Some(95),
            photo_url: None,
        };

        let response = client
            .create_wine_bottle(request)
            .await
            .expect("Failed to create wine bottle")
            .into_inner();

        assert!(response.bottle.is_some());
        let bottle = response.bottle.unwrap();
        assert_eq!(bottle.name, Some("Château Margaux".to_string()));
        assert_eq!(bottle.producer, Some("Château Margaux".to_string()));
        assert_eq!(bottle.grape_variety, vec!["Cabernet Sauvignon".to_string()]);
        assert_eq!(bottle.vintage, Some(2015));
        assert_eq!(bottle.country, Some("France".to_string()));
        assert_eq!(bottle.region, Some("Bordeaux".to_string()));
        assert_eq!(bottle.color, Some(WineColor::Red as i32));
        assert_eq!(bottle.quantity, Some(6));
        assert_eq!(bottle.purchase_price, Some(499.99));
        assert_eq!(bottle.rating, Some(95));
        assert!(bottle.created_at.is_some());
        assert!(bottle.updated_at.is_some());
        assert!(bottle.deleted_at.is_none());
    }

    #[tokio::test]
    async fn test_create_and_get_wine_bottle() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineBottleRequest {
            name: Some("Riesling".to_string()),
            producer: Some("Dr. Loosen".to_string()),
            grape_variety: vec!["Riesling".to_string()],
            vintage: Some(2020),
            country: Some("Germany".to_string()),
            region: Some("Mosel".to_string()),
            color: Some(WineColor::White as i32),
            quantity: Some(12),
            purchase_date: None,
            purchase_price: Some(25.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: Some(2022),
            drink_to_year: Some(2030),
            notes: None,
            rating: Some(88),
            photo_url: None,
        };

        let create_response = client
            .create_wine_bottle(create_request)
            .await
            .expect("Failed to create wine bottle")
            .into_inner();

        let bottle_id = create_response.bottle.unwrap().id;

        let get_request = GetWineBottleRequest {
            id: bottle_id.clone(),
        };

        let get_response = client
            .get_wine_bottle(get_request)
            .await
            .expect("Failed to get wine bottle")
            .into_inner();

        let bottle = get_response.bottle.unwrap();
        assert_eq!(bottle.id, bottle_id);
        assert_eq!(bottle.name, Some("Riesling".to_string()));
        assert_eq!(bottle.vintage, Some(2020));
    }

    #[tokio::test]
    async fn test_get_wine_bottle_not_found() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let request = GetWineBottleRequest {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
        };

        let result = client.get_wine_bottle(request).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_create_update_and_get_wine_bottle() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineBottleRequest {
            name: Some("Pinot Noir".to_string()),
            producer: Some("Domaine de la Romanée-Conti".to_string()),
            grape_variety: vec!["Pinot Noir".to_string()],
            vintage: Some(2018),
            country: Some("France".to_string()),
            region: Some("Burgundy".to_string()),
            color: Some(WineColor::Red as i32),
            quantity: Some(3),
            purchase_date: None,
            purchase_price: Some(350.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: Some(2025),
            drink_to_year: Some(2045),
            notes: Some("Grand Cru".to_string()),
            rating: Some(96),
            photo_url: None,
        };

        let create_response = client
            .create_wine_bottle(create_request)
            .await
            .expect("Failed to create wine bottle")
            .into_inner();

        let bottle_id = create_response.bottle.unwrap().id;

        let update_request = UpdateWineBottleRequest {
            id: bottle_id.clone(),
            name: Some("Romanée-Conti".to_string()),
            producer: None,
            grape_variety: vec![],
            vintage: Some(2019),
            country: None,
            region: None,
            color: None,
            quantity: Some(1),
            purchase_date: None,
            purchase_price: None,
            currency_code: None,
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: Some(100),
            photo_url: None,
            clear_grape_varieties: None,
        };

        let update_response = client
            .update_wine_bottle(update_request)
            .await
            .expect("Failed to update wine bottle")
            .into_inner();

        let updated_bottle = update_response.bottle.unwrap();
        assert_eq!(updated_bottle.id, bottle_id);
        assert_eq!(updated_bottle.name, Some("Romanée-Conti".to_string()));
        assert_eq!(updated_bottle.vintage, Some(2019));
        assert_eq!(updated_bottle.rating, Some(100));
        assert_eq!(updated_bottle.quantity, Some(1));

        let get_request = GetWineBottleRequest { id: bottle_id };

        let get_response = client
            .get_wine_bottle(get_request)
            .await
            .expect("Failed to get wine bottle")
            .into_inner();

        let bottle = get_response.bottle.unwrap();
        assert_eq!(bottle.name, Some("Romanée-Conti".to_string()));
        assert_eq!(bottle.vintage, Some(2019));
    }

    #[tokio::test]
    async fn test_create_delete_and_get_wine_bottle() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineBottleRequest {
            name: Some("Prosecco".to_string()),
            producer: Some("Bisol".to_string()),
            grape_variety: vec!["Glera".to_string()],
            vintage: Some(2021),
            country: Some("Italy".to_string()),
            region: Some("Valdobbiadene".to_string()),
            color: Some(WineColor::Sparkling as i32),
            quantity: Some(24),
            purchase_date: None,
            purchase_price: Some(15.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: Some(2022),
            drink_to_year: Some(2025),
            notes: None,
            rating: Some(85),
            photo_url: None,
        };

        let create_response = client
            .create_wine_bottle(create_request)
            .await
            .expect("Failed to create wine bottle")
            .into_inner();

        let bottle_id = create_response.bottle.unwrap().id;

        let delete_request = DeleteWineBottleRequest {
            id: bottle_id.clone(),
            reason: 1,
        };

        let delete_response = client
            .delete_wine_bottle(delete_request)
            .await
            .expect("Failed to delete wine bottle")
            .into_inner();

        assert!(delete_response.success);

        let get_request = GetWineBottleRequest { id: bottle_id };

        let result = client.get_wine_bottle(get_request).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_delete_wine_bottle_not_found() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let request = DeleteWineBottleRequest {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            reason: 1,
        };

        let result = client.delete_wine_bottle(request).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_list_wine_bottles() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        for i in 1..=5 {
            let request = CreateWineBottleRequest {
                name: Some(format!("Wine {}", i)),
                producer: Some("Test Winery".to_string()),
                grape_variety: vec!["Pinot Noir".to_string()],
                vintage: Some(2020 + i),
                country: Some("France".to_string()),
                region: Some("Burgundy".to_string()),
                color: Some(WineColor::Red as i32),
                quantity: Some(10),
                purchase_date: None,
                purchase_price: Some(50.00 + i as f64),
                currency_code: Some("EUR".to_string()),
                drink_from_year: None,
                drink_to_year: None,
                notes: None,
                rating: Some(85 + i),
                photo_url: None,
            };

            client
                .create_wine_bottle(request)
                .await
                .expect("Failed to create wine bottle");
        }

        let list_request = ListWineBottleRequest {
            filter: None,
            pagination: None,
        };

        let response = client
            .list_wine_bottle(list_request)
            .await
            .expect("Failed to list wine bottles")
            .into_inner();

        assert_eq!(response.bottles.len(), 5);
        assert_eq!(response.total_count, 5);
    }

    #[tokio::test]
    async fn test_list_wine_bottles_excludes_deleted() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineBottleRequest {
            name: Some("To Be Deleted".to_string()),
            producer: Some("Test Winery".to_string()),
            grape_variety: vec!["Chardonnay".to_string()],
            vintage: Some(2020),
            country: Some("France".to_string()),
            region: Some("Burgundy".to_string()),
            color: Some(WineColor::White as i32),
            quantity: Some(10),
            purchase_date: None,
            purchase_price: Some(40.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: Some(87),
            photo_url: None,
        };

        let create_response = client
            .create_wine_bottle(create_request)
            .await
            .expect("Failed to create wine bottle")
            .into_inner();

        let bottle_id = create_response.bottle.unwrap().id;

        let list_request = ListWineBottleRequest {
            filter: None,
            pagination: None,
        };

        let response = client
            .list_wine_bottle(list_request)
            .await
            .expect("Failed to list wine bottles")
            .into_inner();

        assert_eq!(response.bottles.len(), 1);

        let delete_request = DeleteWineBottleRequest {
            id: bottle_id,
            reason: 1,
        };

        client
            .delete_wine_bottle(delete_request)
            .await
            .expect("Failed to delete wine bottle");

        let list_request2 = ListWineBottleRequest {
            filter: None,
            pagination: None,
        };

        let response2 = client
            .list_wine_bottle(list_request2)
            .await
            .expect("Failed to list wine bottles")
            .into_inner();

        assert_eq!(response2.bottles.len(), 0);
        assert_eq!(response2.total_count, 0);
    }

    #[tokio::test]
    async fn test_update_wine_bottle_not_found() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let request = UpdateWineBottleRequest {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            name: Some("New Name".to_string()),
            producer: None,
            grape_variety: vec![],
            vintage: None,
            country: None,
            region: None,
            color: None,
            quantity: None,
            purchase_date: None,
            purchase_price: None,
            currency_code: None,
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: None,
            photo_url: None,
            clear_grape_varieties: None,
        };

        let result = client.update_wine_bottle(request).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_update_clear_grape_varieties() {
        let addr = setup_test_server().await;
        let mut client = WineBottleServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineBottleRequest {
            name: Some("Chablis".to_string()),
            producer: Some("William Fèvre".to_string()),
            grape_variety: vec!["Chardonnay".to_string()],
            vintage: Some(2020),
            country: Some("France".to_string()),
            region: Some("Burgundy".to_string()),
            color: Some(WineColor::White as i32),
            quantity: Some(6),
            purchase_date: None,
            purchase_price: Some(45.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: None,
            photo_url: None,
        };

        let create_response = client
            .create_wine_bottle(create_request)
            .await
            .expect("Failed to create wine bottle")
            .into_inner();

        let bottle_id = create_response.bottle.unwrap().id;

        let get_request = GetWineBottleRequest {
            id: bottle_id.clone(),
        };
        let get_response = client
            .get_wine_bottle(get_request)
            .await
            .expect("Failed to get wine bottle")
            .into_inner();
        let bottle = get_response.bottle.unwrap();
        assert_eq!(bottle.grape_variety, vec!["Chardonnay".to_string()]);

        let update_request = UpdateWineBottleRequest {
            id: bottle_id.clone(),
            name: None,
            producer: None,
            grape_variety: vec![],
            vintage: None,
            country: None,
            region: None,
            color: None,
            quantity: None,
            purchase_date: None,
            purchase_price: None,
            currency_code: None,
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: None,
            photo_url: None,
            clear_grape_varieties: Some(true),
        };

        let update_response = client
            .update_wine_bottle(update_request)
            .await
            .expect("Failed to update wine bottle")
            .into_inner();

        let updated_bottle = update_response.bottle.unwrap();
        assert_eq!(updated_bottle.grape_variety, Vec::<String>::new());

        let get_request = GetWineBottleRequest { id: bottle_id };
        let get_response = client
            .get_wine_bottle(get_request)
            .await
            .expect("Failed to get wine bottle")
            .into_inner();
        let bottle = get_response.bottle.unwrap();
        assert_eq!(bottle.grape_variety, Vec::<String>::new());
    }
}

mod wine_cellar_tests {
    use super::*;

    #[tokio::test]
    async fn test_create_wine_cellar() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let request = CreateWineCellarRequest {
            name: "Main Cellar".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        let response = cellar_client
            .create_wine_cellar(request)
            .await
            .expect("Failed to create wine cellar")
            .into_inner();

        assert!(response.wine_cellar.is_some());
        let cellar = response.wine_cellar.unwrap();
        assert_eq!(cellar.name, Some("Main Cellar".to_string()));
        assert!(!cellar.id.is_empty());
    }

    #[tokio::test]
    async fn test_create_wine_cellar_with_new_bottles() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let new_bottle = CreateWineBottleRequest {
            name: Some("New Bottle in Cellar".to_string()),
            producer: Some("Test Producer".to_string()),
            grape_variety: vec!["Cabernet Sauvignon".to_string()],
            vintage: Some(2019),
            country: Some("France".to_string()),
            region: Some("Bordeaux".to_string()),
            color: Some(WineColor::Red as i32),
            quantity: Some(6),
            purchase_date: None,
            purchase_price: Some(100.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: Some(90),
            photo_url: None,
        };

        let request = CreateWineCellarRequest {
            name: "Bordeaux Collection".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![new_bottle],
        };

        let response = cellar_client
            .create_wine_cellar(request)
            .await
            .expect("Failed to create wine cellar")
            .into_inner();

        let cellar = response.wine_cellar.unwrap();
        assert_eq!(cellar.name, Some("Bordeaux Collection".to_string()));
        assert_eq!(cellar.bottles.len(), 1);
        assert_eq!(cellar.bottles[0].name, "New Bottle in Cellar");
    }

    #[tokio::test]
    async fn test_create_and_update_wine_cellar() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineCellarRequest {
            name: "Original Name".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        let create_response = cellar_client
            .create_wine_cellar(create_request)
            .await
            .expect("Failed to create wine cellar")
            .into_inner();

        let cellar_id = create_response.wine_cellar.unwrap().id;

        let update_request = UpdateWineCellarRequest {
            id: cellar_id.clone(),
            name: Some("Updated Name".to_string()),
            bottle_ids: vec![],
        };

        let update_response = cellar_client
            .update_wine_cellar(update_request)
            .await
            .expect("Failed to update wine cellar")
            .into_inner();

        let updated_cellar = update_response.wine_cellar.unwrap();
        assert_eq!(updated_cellar.id, cellar_id);
        assert_eq!(updated_cellar.name, Some("Updated Name".to_string()));
    }

    #[tokio::test]
    async fn test_create_and_delete_wine_cellar() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let create_request = CreateWineCellarRequest {
            name: "To Be Deleted".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        let create_response = cellar_client
            .create_wine_cellar(create_request)
            .await
            .expect("Failed to create wine cellar")
            .into_inner();

        let cellar_id = create_response.wine_cellar.unwrap().id;

        let delete_request = DeleteWineCellarRequest {
            id: cellar_id.clone(),
        };

        let delete_response = cellar_client
            .delete_wine_cellar(delete_request)
            .await
            .expect("Failed to delete wine cellar")
            .into_inner();

        assert!(delete_response.success);

        let update_request = UpdateWineCellarRequest {
            id: cellar_id,
            name: Some("Should Fail".to_string()),
            bottle_ids: vec![],
        };

        let result = cellar_client.update_wine_cellar(update_request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_cellar_bottle_associations() {
        let addr = setup_test_server().await;
        let mut bottle_client = WineBottleServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");
        let mut cellar_client = WineCellarServiceClient::connect(addr)
            .await
            .expect("Failed to connect to server");

        let bottle1 = CreateWineBottleRequest {
            name: Some("Bottle 1".to_string()),
            producer: Some("Winery".to_string()),
            grape_variety: vec!["Merlot".to_string()],
            vintage: Some(2020),
            country: Some("France".to_string()),
            region: Some("Bordeaux".to_string()),
            color: Some(WineColor::Red as i32),
            quantity: Some(6),
            purchase_date: None,
            purchase_price: Some(30.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: Some(88),
            photo_url: None,
        };

        let bottle2 = CreateWineBottleRequest {
            name: Some("Bottle 2".to_string()),
            producer: Some("Winery".to_string()),
            grape_variety: vec!["Cabernet Sauvignon".to_string()],
            vintage: Some(2019),
            country: Some("France".to_string()),
            region: Some("Bordeaux".to_string()),
            color: Some(WineColor::Red as i32),
            quantity: Some(6),
            purchase_date: None,
            purchase_price: Some(40.00),
            currency_code: Some("EUR".to_string()),
            drink_from_year: None,
            drink_to_year: None,
            notes: None,
            rating: Some(90),
            photo_url: None,
        };

        let bottle1_response = bottle_client
            .create_wine_bottle(bottle1)
            .await
            .expect("Failed to create bottle 1")
            .into_inner();
        let bottle2_response = bottle_client
            .create_wine_bottle(bottle2)
            .await
            .expect("Failed to create bottle 2")
            .into_inner();

        let bottle1_id = bottle1_response
            .bottle
            .expect("Bottle 1 should be created")
            .id
            .clone();
        let bottle2_id = bottle2_response
            .bottle
            .expect("Bottle 2 should be created")
            .id
            .clone();

        let cellar_request = CreateWineCellarRequest {
            name: "Test Cellar".to_string(),
            existing_bottle_ids: vec![bottle1_id.clone()],
            new_bottles: vec![],
        };

        let cellar_response = cellar_client
            .create_wine_cellar(cellar_request)
            .await
            .expect("Failed to create cellar")
            .into_inner();

        let wine_cellar = cellar_response
            .wine_cellar
            .expect("Cellar should be created");
        let cellar_id = wine_cellar.id.clone();
        assert_eq!(wine_cellar.bottles.len(), 1);

        let update_request = UpdateWineCellarRequest {
            id: cellar_id.clone(),
            name: None,
            bottle_ids: vec![bottle1_id.clone(), bottle2_id.clone()],
        };

        let update_response = cellar_client
            .update_wine_cellar(update_request)
            .await
            .expect("Failed to update cellar")
            .into_inner();

        assert_eq!(
            update_response.wine_cellar.as_ref().unwrap().bottles.len(),
            2
        );
    }

    #[tokio::test]
    async fn test_get_wine_cellar() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let request = CreateWineCellarRequest {
            name: "Test Cellar".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        let create_response = cellar_client
            .create_wine_cellar(request)
            .await
            .expect("Failed to create wine cellar")
            .into_inner();

        let cellar_id = create_response.wine_cellar.unwrap().id;

        let get_request = GetWineCellarRequest {
            id: cellar_id.clone(),
        };

        let get_response = cellar_client
            .get_wine_cellar(get_request)
            .await
            .expect("Failed to get wine cellar")
            .into_inner();

        assert!(get_response.wine_cellar.is_some());
        let cellar = get_response.wine_cellar.unwrap();
        assert_eq!(cellar.id, cellar_id);
        assert_eq!(cellar.name, Some("Test Cellar".to_string()));
    }

    #[tokio::test]
    async fn test_get_wine_cellar_not_found() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let get_request = GetWineCellarRequest {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
        };

        let result = cellar_client.get_wine_cellar(get_request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_wine_cellars() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let request1 = CreateWineCellarRequest {
            name: "Cellar Alpha".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        let request2 = CreateWineCellarRequest {
            name: "Cellar Beta".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        cellar_client
            .create_wine_cellar(request1)
            .await
            .expect("Failed to create cellar 1");

        cellar_client
            .create_wine_cellar(request2)
            .await
            .expect("Failed to create cellar 2");

        let list_request = ListWineCellarRequest {
            name_contains: None,
            pagination: Some(PaginationParams {
                limit: 50,
                offset: 0,
                cursor: None,
            }),
        };

        let list_response = cellar_client
            .list_wine_cellar(list_request)
            .await
            .expect("Failed to list wine cellars")
            .into_inner();

        assert_eq!(list_response.total_count, 2);
        assert_eq!(list_response.wine_cellar.len(), 2);
    }

    #[tokio::test]
    async fn test_list_wine_cellars_filter_by_name() {
        let addr = setup_test_server().await;
        let mut cellar_client = WineCellarServiceClient::connect(addr.clone())
            .await
            .expect("Failed to connect to server");

        let request1 = CreateWineCellarRequest {
            name: "Cellar Alpha".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        let request2 = CreateWineCellarRequest {
            name: "Cellar Beta".to_string(),
            existing_bottle_ids: vec![],
            new_bottles: vec![],
        };

        cellar_client
            .create_wine_cellar(request1)
            .await
            .expect("Failed to create cellar 1");

        cellar_client
            .create_wine_cellar(request2)
            .await
            .expect("Failed to create cellar 2");

        let list_request = ListWineCellarRequest {
            name_contains: Some("Alpha".to_string()),
            pagination: Some(PaginationParams {
                limit: 50,
                offset: 0,
                cursor: None,
            }),
        };

        let list_response = cellar_client
            .list_wine_cellar(list_request)
            .await
            .expect("Failed to list wine cellars")
            .into_inner();

        assert_eq!(list_response.total_count, 1);
        assert_eq!(list_response.wine_cellar.len(), 1);
        assert_eq!(
            list_response.wine_cellar[0].name,
            Some("Cellar Alpha".to_string())
        );
    }
}
