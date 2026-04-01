use chrono::Datelike;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::db::DbPool;
use crate::db::models::{
    WineBottle as ModelWineBottle, WineCellar as ModelWineCellar, WineColor as ModelWineColor,
    get_or_create_variety, get_varieties_for_bottle, set_bottle_varieties,
};
use crate::errors::ServiceError;
use crate::generated::cellar::{
    CreateWineBottleRequest, CreateWineBottleResponse, CreateWineCellarRequest,
    CreateWineCellarResponse, DeleteWineBottleRequest, DeleteWineBottleResponse,
    DeleteWineCellarRequest, DeleteWineCellarResponse, GetWineBottleRequest, GetWineBottleResponse,
    GetWineCellarRequest, GetWineCellarResponse, ListWineBottleRequest, ListWineBottleResponse,
    ListWineCellarRequest, ListWineCellarResponse, UpdateWineBottleRequest,
    UpdateWineBottleResponse, UpdateWineCellarRequest, UpdateWineCellarResponse, WineBottleDetail,
    WineBottleSummary, WineCellar as ProtoWineCellar,
    wine_bottle_service_server::WineBottleService, wine_cellar_service_server::WineCellarService,
};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DbPool>,
}

impl AppState {
    pub fn new(db: Arc<DbPool>) -> Self {
        Self { db }
    }
}

fn parse_naive_date(s: &str) -> Result<chrono::NaiveDate, ServiceError> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| ServiceError::InvalidArgument(format!("Invalid date format: {}", s)))
}

fn parse_naive_date_opt(s: &Option<String>) -> Option<chrono::NaiveDate> {
    match s {
        Some(date_str) if !date_str.is_empty() => parse_naive_date(date_str).ok(),
        _ => None,
    }
}

fn parse_uuid_row(row: &rusqlite::Row, idx: usize) -> Result<Uuid, rusqlite::Error> {
    let s: String = row.get(idx)?;
    Uuid::parse_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            s.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid UUID: {}", e),
            )),
        )
    })
}

fn parse_iso_row(
    row: &rusqlite::Row,
    idx: usize,
) -> Result<chrono::DateTime<chrono::Utc>, rusqlite::Error> {
    let s: String = row.get(idx)?;
    s.parse::<chrono::DateTime<chrono::Utc>>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            s.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid ISO date format",
            )),
        )
    })
}

fn bottle_to_proto(bottle: &ModelWineBottle) -> WineBottleDetail {
    WineBottleDetail {
        id: bottle.id.to_string(),
        name: bottle.name.clone(),
        producer: bottle.producer.clone(),
        grape_variety: bottle.grape_variety.clone(),
        vintage: bottle.vintage,
        country: bottle.country.clone(),
        region: bottle.region.clone(),
        color: Some(bottle.color as i32),
        quantity: bottle.quantity,
        purchase_date: bottle.purchase_date.map(|d| d.to_string()),
        purchase_price: bottle.purchase_price,
        currency_code: bottle.currency_code.clone(),
        drink_from_year: bottle.drink_from_year,
        drink_to_year: bottle.drink_to_year,
        notes: bottle.notes.clone(),
        rating: bottle.rating,
        photo_url: bottle.photo_url.clone(),
        created_at: Some(bottle.created_at.to_rfc3339()),
        updated_at: Some(bottle.updated_at.to_rfc3339()),
        deleted_at: bottle.deleted_at.map(|d| d.to_rfc3339()),
    }
}

fn proto_to_bottle(request: &CreateWineBottleRequest) -> Result<ModelWineBottle, ServiceError> {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();

    let grape_variety = if request.grape_variety.is_empty() {
        Vec::new()
    } else {
        request.grape_variety.clone()
    };

    let color = request
        .color
        .map(ModelWineColor::from)
        .unwrap_or(ModelWineColor::Unspecified);

    let purchase_date = match &request.purchase_date {
        Some(date_str) if !date_str.is_empty() => Some(parse_naive_date(date_str)?),
        _ => None,
    };

    Ok(ModelWineBottle {
        id,
        name: request.name.clone(),
        producer: request.producer.clone(),
        grape_variety,
        vintage: request.vintage,
        country: request.country.clone(),
        region: request.region.clone(),
        color,
        quantity: request.quantity,
        purchase_date,
        purchase_price: request.purchase_price,
        currency_code: request.currency_code.clone(),
        drink_from_year: request.drink_from_year,
        drink_to_year: request.drink_to_year,
        notes: request.notes.clone(),
        rating: request.rating,
        photo_url: request.photo_url.clone(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
    })
}

fn run_transaction<T>(
    conn: &Connection,
    f: impl FnOnce(&Transaction) -> Result<T, ServiceError>,
) -> Result<T, ServiceError> {
    let tx = conn.unchecked_transaction().map_err(ServiceError::from)?;
    let result = f(&tx)?;
    tx.commit().map_err(ServiceError::from)?;
    Ok(result)
}

#[tonic::async_trait]
impl WineCellarService for AppState {
    async fn create_wine_cellar(
        &self,
        request: Request<CreateWineCellarRequest>,
    ) -> Result<Response<CreateWineCellarResponse>, Status> {
        let req = request.into_inner();
        let now = chrono::Utc::now();
        let cellar_id = Uuid::new_v4();
        let cellar_name = req.name.clone();
        let new_bottles = req.new_bottles.clone();
        let existing_bottle_ids = req.existing_bottle_ids.clone();

        let proto_cellar = self
            .db
            .execute_async(move |conn| {
                run_transaction(conn, |tx| {
                    tx.execute(
                        "INSERT INTO wine_cellars (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            cellar_id.to_string(),
                            cellar_name,
                            now.to_rfc3339(),
                            now.to_rfc3339()
                        ],
                    )
                    .map_err(ServiceError::from)?;

                    for bottle_req in &new_bottles {
                        let mut bottle = proto_to_bottle(bottle_req)?;
                        bottle.id = Uuid::new_v4();

                        tx.execute(
                            "INSERT INTO wine_bottles (id, name, producer, grape_variety, vintage, country, region, color, quantity, purchase_date, purchase_price, currency_code, drink_from_year, drink_to_year, notes, rating, photo_url, created_at, updated_at, deleted_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                            params![
                                bottle.id.to_string(),
                                bottle.name,
                                bottle.producer,
                                ModelWineBottle::grape_variety_to_string(&bottle.grape_variety),
                                bottle.vintage,
                                bottle.country,
                                bottle.region,
                                i32::from(bottle.color),
                                bottle.quantity,
                                bottle.purchase_date.map(|d| d.to_string()),
                                bottle.purchase_price,
                                bottle.currency_code,
                                bottle.drink_from_year,
                                bottle.drink_to_year,
                                bottle.notes,
                                bottle.rating,
                                bottle.photo_url,
                                bottle.created_at.to_rfc3339(),
                                bottle.updated_at.to_rfc3339(),
                                bottle.deleted_at.map(|d| d.to_rfc3339()),
                            ],
                        )
                        .map_err(ServiceError::from)?;

                        for variety_name in &bottle.grape_variety {
                            let variety_id = get_or_create_variety(tx, variety_name)?;
                            tx.execute(
                                "INSERT OR IGNORE INTO wine_bottle_varieties (bottle_id, variety_id) VALUES (?1, ?2)",
                                params![bottle.id.to_string(), variety_id.to_string()],
                            )
                            .map_err(ServiceError::from)?;
                        }

                        tx.execute(
                            "INSERT INTO wine_cellar_bottles (cellar_id, bottle_id, added_at) VALUES (?1, ?2, ?3)",
                            params![cellar_id.to_string(), bottle.id.to_string(), now.to_rfc3339()],
                        )
                        .map_err(ServiceError::from)?;
                    }

                    for bottle_id in &existing_bottle_ids {
                        tx.execute(
                            "INSERT INTO wine_cellar_bottles (cellar_id, bottle_id, added_at) VALUES (?1, ?2, ?3)",
                            params![cellar_id.to_string(), bottle_id, now.to_rfc3339()],
                        )
                        .map_err(ServiceError::from)?;
                    }

                    let bottles = fetch_cellar_bottles(tx, &cellar_id)?;

                    Ok(ProtoWineCellar {
                        id: cellar_id.to_string(),
                        name: Some(req.name.clone()),
                        bottles,
                    })
                })
            })
            .await?;

        Ok(Response::new(CreateWineCellarResponse {
            wine_cellar: Some(proto_cellar),
        }))
    }

    async fn update_wine_cellar(
        &self,
        request: Request<UpdateWineCellarRequest>,
    ) -> Result<Response<UpdateWineCellarResponse>, Status> {
        let req = request.into_inner();
        let now = chrono::Utc::now();

        let cellar_uuid =
            Uuid::parse_str(&req.id).map_err(|_| Status::invalid_argument("Invalid cellar ID"))?;

        let req_name = req.name.clone();
        let req_bottle_ids = req.bottle_ids.clone();

        let proto_cellar = self
            .db
            .execute_async(move |conn| {
                run_transaction(conn, |tx| {
                    if let Some(ref name) = req_name {
                        let rows = tx
                            .execute(
                                "UPDATE wine_cellars SET name = ?1, updated_at = ?2 WHERE id = ?3",
                                params![name, now.to_rfc3339(), cellar_uuid.to_string()],
                            )
                            .map_err(ServiceError::from)?;

                        if rows == 0 {
                            return Err(ServiceError::NotFound("Cellar not found".to_string()));
                        }
                    }

                    if !req_bottle_ids.is_empty() {
                        let existing_bottles: Vec<String> = {
                            let mut stmt = tx
                                .prepare("SELECT bottle_id FROM wine_cellar_bottles WHERE cellar_id = ?1")
                                .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;
                            stmt.query_map(params![cellar_uuid.to_string()], |row| row.get(0))
                                .map_err(ServiceError::from)?
                                .filter_map(|r| r.ok())
                                .collect()
                        };

                        let new_ids: std::collections::HashSet<_> = req_bottle_ids.iter().collect();
                        let existing_ids: std::collections::HashSet<_> = existing_bottles.iter().collect();

                        let to_remove: Vec<_> = existing_ids.difference(&new_ids).cloned().collect();
                        let to_add: Vec<_> = new_ids.difference(&existing_ids).cloned().collect();

                        for bottle_id in &to_remove {
                            tx.execute(
                                "DELETE FROM wine_cellar_bottles WHERE cellar_id = ?1 AND bottle_id = ?2",
                                params![cellar_uuid.to_string(), bottle_id],
                            )
                            .map_err(ServiceError::from)?;
                        }

                        for bottle_id in &to_add {
                            tx.execute(
                                "INSERT INTO wine_cellar_bottles (cellar_id, bottle_id, added_at) VALUES (?1, ?2, ?3)",
                                params![cellar_uuid.to_string(), bottle_id, now.to_rfc3339()],
                            )
                            .map_err(ServiceError::from)?;
                        }
                    }

                    let mut stmt = conn
                        .prepare("SELECT id, name, created_at, updated_at FROM wine_cellars WHERE id = ?1")
                        .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

                    let cellar = stmt
                        .query_row(params![cellar_uuid.to_string()], |row| {
                            Ok(ModelWineCellar {
                                id: parse_uuid_row(row, 0)?,
                                name: row.get(1)?,
                                created_at: parse_iso_row(row, 2)?,
                                updated_at: parse_iso_row(row, 3)?,
                            })
                        })
                        .map_err(|_| ServiceError::NotFound("Cellar not found".to_string()))?;

                    let bottles = fetch_cellar_bottles(conn, &cellar_uuid)?;

                    Ok(ProtoWineCellar {
                        id: cellar.id.to_string(),
                        name: cellar.name,
                        bottles,
                    })
                })
            })
            .await?;

        Ok(Response::new(UpdateWineCellarResponse {
            wine_cellar: Some(proto_cellar),
        }))
    }

    async fn delete_wine_cellar(
        &self,
        request: Request<DeleteWineCellarRequest>,
    ) -> Result<Response<DeleteWineCellarResponse>, Status> {
        let req = request.into_inner();

        let cellar_uuid =
            Uuid::parse_str(&req.id).map_err(|_| Status::invalid_argument("Invalid cellar ID"))?;

        let _ = self
            .db
            .execute_async(move |conn| {
                run_transaction(conn, |tx| {
                    tx.execute(
                        "DELETE FROM wine_cellar_bottles WHERE cellar_id = ?1",
                        params![cellar_uuid.to_string()],
                    )
                    .map_err(ServiceError::from)?;

                    let rows = tx
                        .execute(
                            "DELETE FROM wine_cellars WHERE id = ?1",
                            params![cellar_uuid.to_string()],
                        )
                        .map_err(ServiceError::from)?;

                    if rows == 0 {
                        return Err(ServiceError::NotFound("Cellar not found".to_string()));
                    }

                    Ok(())
                })
            })
            .await?;

        Ok(Response::new(DeleteWineCellarResponse { success: true }))
    }

    async fn get_wine_cellar(
        &self,
        request: Request<GetWineCellarRequest>,
    ) -> Result<Response<GetWineCellarResponse>, Status> {
        let req = request.into_inner();

        let cellar_uuid =
            Uuid::parse_str(&req.id).map_err(|_| Status::invalid_argument("Invalid cellar ID"))?;

        let proto_cellar = self
            .db
            .execute_async(move |conn| {
                let cellar = fetch_cellar(conn, &cellar_uuid)?
                    .ok_or_else(|| ServiceError::NotFound("Cellar not found".to_string()))?;

                let bottles = fetch_cellar_bottles(conn, &cellar_uuid)?;

                Ok(ProtoWineCellar {
                    id: cellar.id.to_string(),
                    name: cellar.name,
                    bottles,
                })
            })
            .await?;

        Ok(Response::new(GetWineCellarResponse {
            wine_cellar: Some(proto_cellar),
        }))
    }

    async fn list_wine_cellar(
        &self,
        request: Request<ListWineCellarRequest>,
    ) -> Result<Response<ListWineCellarResponse>, Status> {
        let req = request.into_inner();

        let name_contains = req.name_contains.clone();
        let pagination = req
            .pagination
            .unwrap_or(crate::generated::cellar::PaginationParams {
                limit: 50,
                offset: 0,
                cursor: None,
            });

        let result = self
            .db
            .execute_async(move |conn| {
                let limit = pagination.limit.clamp(1, 100);
                let offset = pagination.offset.max(0);

                let (count_sql, count_params): (&str, Vec<Box<dyn rusqlite::ToSql>>) =
                    if let Some(ref name) = name_contains {
                        (
                            "SELECT COUNT(*) FROM wine_cellars WHERE name LIKE ?",
                            vec![Box::new(format!("%{}%", name))],
                        )
                    } else {
                        ("SELECT COUNT(*) FROM wine_cellars", vec![])
                    };

                let count_params_refs: Vec<&dyn rusqlite::ToSql> =
                    count_params.iter().map(|p| p.as_ref()).collect();
                let total_count: i32 = conn
                    .query_row(count_sql, count_params_refs.as_slice(), |row| row.get(0))
                    .map_err(|e| {
                        ServiceError::Internal(format!("Failed to count cellars: {}", e))
                    })?;

                let (query_sql, query_params): (&str, Vec<Box<dyn rusqlite::ToSql>>) =
                    if let Some(ref name) = name_contains {
                        (
                            "SELECT id, name, created_at, updated_at FROM wine_cellars WHERE name LIKE ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
                            vec![
                                Box::new(format!("%{}%", name)),
                                Box::new(limit),
                                Box::new(offset),
                            ],
                        )
                    } else {
                        (
                            "SELECT id, name, created_at, updated_at FROM wine_cellars ORDER BY created_at DESC LIMIT ? OFFSET ?",
                            vec![Box::new(limit), Box::new(offset)],
                        )
                    };

                let query_params_refs: Vec<&dyn rusqlite::ToSql> =
                    query_params.iter().map(|p| p.as_ref()).collect();

                let mut stmt = conn
                    .prepare(query_sql)
                    .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

                let mut rows = stmt
                    .query(query_params_refs.as_slice())
                    .map_err(|e| ServiceError::Internal(format!("Failed to query cellars: {}", e)))?;

                let mut cellars = Vec::new();
                while let Some(row) = rows.next().map_err(|e| {
                    ServiceError::Internal(format!("Failed to read row: {}", e))
                })? {
                    let id_str: String =
                        row.get(0)
                            .map_err(|e| ServiceError::Internal(format!("Failed to get id: {}", e)))?;
                    let uuid = Uuid::parse_str(&id_str).map_err(|e| {
                        ServiceError::Internal(format!("Invalid UUID: {}", e))
                    })?;
                    let name: Option<String> = row
                        .get(1)
                        .map_err(|e| ServiceError::Internal(format!("Failed to get name: {}", e)))?;

                    let bottles =
                        fetch_cellar_bottles(conn, &uuid).map_err(|e| ServiceError::Internal(format!("Failed to fetch bottles: {}", e)))?;

                    cellars.push(ProtoWineCellar {
                        id: uuid.to_string(),
                        name,
                        bottles,
                    });
                }

                Ok(ListWineCellarResponse {
                    wine_cellar: cellars,
                    total_count,
                    next_cursor: None,
                })
            })
            .await?;

        Ok(Response::new(result))
    }
}

fn fetch_cellar(
    db: &Connection,
    cellar_id: &Uuid,
) -> Result<Option<ModelWineCellar>, ServiceError> {
    let mut stmt = db
        .prepare("SELECT id, name, created_at, updated_at FROM wine_cellars WHERE id = ?1")
        .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

    let result = stmt
        .query_row(params![cellar_id.to_string()], |row| {
            Ok(ModelWineCellar {
                id: parse_uuid_row(row, 0)?,
                name: row.get(1)?,
                created_at: parse_iso_row(row, 2)?,
                updated_at: parse_iso_row(row, 3)?,
            })
        })
        .optional()
        .map_err(|e| ServiceError::Internal(format!("Failed to query cellar: {}", e)))?;

    Ok(result)
}

fn fetch_cellar_bottles(
    db: &Connection,
    cellar_id: &Uuid,
) -> Result<Vec<WineBottleSummary>, ServiceError> {
    let mut stmt = db
        .prepare(
            "SELECT b.id, b.name, b.producer, b.vintage, b.country, b.region, b.color
             FROM wine_bottles b
             INNER JOIN wine_cellar_bottles cb ON b.id = cb.bottle_id
             WHERE cb.cellar_id = ?1 AND b.deleted_at IS NULL",
        )
        .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

    let bottle_ids: Vec<String> = stmt
        .query_map(params![cellar_id.to_string()], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut result = Vec::new();
    for bottle_id in bottle_ids {
        let uuid = Uuid::parse_str(&bottle_id)
            .map_err(|e| ServiceError::Internal(format!("Invalid bottle UUID: {}", e)))?;
        let varieties = get_varieties_for_bottle(db, &uuid).map_err(ServiceError::from)?;

        let mut stmt = db
            .prepare(
                "SELECT id, name, producer, vintage, country, region, color
                 FROM wine_bottles WHERE id = ?1 AND deleted_at IS NULL",
            )
            .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

        let summary = stmt
            .query_row(params![bottle_id], |row| {
                Ok(WineBottleSummary {
                    id: row.get(0)?,
                    name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    producer: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    grape_variety: varieties.clone(),
                    vintage: row.get::<_, Option<i32>>(3)?.unwrap_or(0),
                    country: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    region: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    color: row.get::<_, i32>(6)?,
                })
            })
            .map_err(|e| ServiceError::Internal(format!("Failed to query bottle: {}", e)))?;
        result.push(summary);
    }

    Ok(result)
}

fn fetch_bottle_detail(
    db: &Connection,
    bottle_id: &Uuid,
) -> Result<Option<ModelWineBottle>, ServiceError> {
    let mut stmt = db
        .prepare(
            "SELECT id, name, producer, vintage, country, region, color,
                    quantity, purchase_date, purchase_price, currency_code,
                    drink_from_year, drink_to_year, notes, rating, photo_url,
                    created_at, updated_at, deleted_at
             FROM wine_bottles WHERE id = ?1",
        )
        .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

    let mut bottle = stmt
        .query_row(params![bottle_id.to_string()], |row| {
            let purchase_date_str: Option<String> = row.get(8)?;
            let deleted_at_str: Option<String> = row.get(18)?;

            let purchase_date = parse_naive_date_opt(&purchase_date_str);

            let deleted_at = match deleted_at_str {
                Some(ref s) if !s.is_empty() => {
                    Some(s.parse::<chrono::DateTime<chrono::Utc>>().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            s.len(),
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Invalid deleted_at timestamp",
                            )),
                        )
                    })?)
                }
                _ => None,
            };

            Ok(ModelWineBottle {
                id: parse_uuid_row(row, 0)?,
                name: row.get(1)?,
                producer: row.get(2)?,
                grape_variety: Vec::new(),
                vintage: row.get(3)?,
                country: row.get(4)?,
                region: row.get(5)?,
                color: ModelWineColor::from(row.get::<_, i32>(6)?),
                quantity: row.get(7)?,
                purchase_date,
                purchase_price: row.get(9)?,
                currency_code: row.get(10)?,
                drink_from_year: row.get(11)?,
                drink_to_year: row.get(12)?,
                notes: row.get(13)?,
                rating: row.get(14)?,
                photo_url: row.get(15)?,
                created_at: parse_iso_row(row, 16)?,
                updated_at: parse_iso_row(row, 17)?,
                deleted_at,
            })
        })
        .optional()
        .map_err(|e| ServiceError::Internal(format!("Failed to query bottle: {}", e)))?;

    if let Some(ref mut bottle) = bottle {
        bottle.grape_variety =
            get_varieties_for_bottle(db, bottle_id).map_err(ServiceError::from)?;
    }

    Ok(bottle)
}

fn get_sort_column(sort_by: i32) -> &'static str {
    match sort_by {
        1 => "name",
        2 => "vintage",
        3 => "rating",
        4 => "purchase_date",
        5 => "quantity",
        6 => "created_at",
        7 => "producer",
        8 => "country",
        9 => "region",
        _ => "name",
    }
}

#[tonic::async_trait]
impl WineBottleService for AppState {
    async fn create_wine_bottle(
        &self,
        request: Request<CreateWineBottleRequest>,
    ) -> Result<Response<CreateWineBottleResponse>, Status> {
        let req = request.into_inner();
        let bottle = proto_to_bottle(&req).map_err(Status::from)?;

        let bottle_id_str = bottle.id.to_string();
        let bottle_name = bottle.name.clone();
        let bottle_producer = bottle.producer.clone();
        let bottle_grape_str = ModelWineBottle::grape_variety_to_string(&bottle.grape_variety);
        let bottle_vintage = bottle.vintage;
        let bottle_country = bottle.country.clone();
        let bottle_region = bottle.region.clone();
        let bottle_color = i32::from(bottle.color);
        let bottle_quantity = bottle.quantity;
        let bottle_purchase_date = bottle.purchase_date.map(|d| d.to_string());
        let bottle_purchase_price = bottle.purchase_price;
        let bottle_currency_code = bottle.currency_code.clone();
        let bottle_drink_from_year = bottle.drink_from_year;
        let bottle_drink_to_year = bottle.drink_to_year;
        let bottle_notes = bottle.notes.clone();
        let bottle_rating = bottle.rating;
        let bottle_photo_url = bottle.photo_url.clone();
        let bottle_created_at = bottle.created_at.to_rfc3339();
        let bottle_updated_at = bottle.updated_at.to_rfc3339();

        self.db
            .execute(move |conn| {
                conn.execute(
                    "INSERT INTO wine_bottles (id, name, producer, grape_variety, vintage, country, region, color, quantity, purchase_date, purchase_price, currency_code, drink_from_year, drink_to_year, notes, rating, photo_url, created_at, updated_at, deleted_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                    params![
                        bottle_id_str,
                        bottle_name,
                        bottle_producer,
                        bottle_grape_str,
                        bottle_vintage,
                        bottle_country,
                        bottle_region,
                        bottle_color,
                        bottle_quantity,
                        bottle_purchase_date,
                        bottle_purchase_price,
                        bottle_currency_code,
                        bottle_drink_from_year,
                        bottle_drink_to_year,
                        bottle_notes,
                        bottle_rating,
                        bottle_photo_url,
                        bottle_created_at,
                        bottle_updated_at,
                        Option::<String>::None,
                    ],
                )
            })
            .await?;

        for variety_name in &bottle.grape_variety {
            let variety_name = variety_name.clone();
            let bottle_id = bottle.id.to_string();
            let variety_id = self
                .db
                .execute_async(move |conn| get_or_create_variety(conn, &variety_name))
                .await?;
            self.db
                .execute(move |conn| {
                    conn.execute(
                        "INSERT OR IGNORE INTO wine_bottle_varieties (bottle_id, variety_id) VALUES (?1, ?2)",
                        params![bottle_id, variety_id.to_string()],
                    )
                })
                .await?;
        }

        Ok(Response::new(CreateWineBottleResponse {
            bottle: Some(bottle_to_proto(&bottle)),
        }))
    }

    async fn get_wine_bottle(
        &self,
        request: Request<GetWineBottleRequest>,
    ) -> Result<Response<GetWineBottleResponse>, Status> {
        let req = request.into_inner();

        let bottle_uuid =
            Uuid::parse_str(&req.id).map_err(|_| Status::invalid_argument("Invalid bottle ID"))?;

        let bottle = self
            .db
            .execute_async(move |conn| {
                fetch_bottle_detail(conn, &bottle_uuid)?
                    .ok_or_else(|| ServiceError::NotFound("Bottle not found".to_string()))
            })
            .await?;

        if bottle.deleted_at.is_some() {
            return Err(Status::not_found("Bottle not found"));
        }

        Ok(Response::new(GetWineBottleResponse {
            bottle: Some(bottle_to_proto(&bottle)),
        }))
    }

    async fn update_wine_bottle(
        &self,
        request: Request<UpdateWineBottleRequest>,
    ) -> Result<Response<UpdateWineBottleResponse>, Status> {
        let req = request.into_inner();
        let now = chrono::Utc::now();

        let bottle_uuid =
            Uuid::parse_str(&req.id).map_err(|_| Status::invalid_argument("Invalid bottle ID"))?;

        let req_name = req.name.clone();
        let req_producer = req.producer.clone();
        let req_grape_variety = req.grape_variety.clone();
        let req_vintage = req.vintage;
        let req_country = req.country.clone();
        let req_region = req.region.clone();
        let req_color = req.color;
        let req_quantity = req.quantity;
        let req_purchase_date = req.purchase_date.clone();
        let req_purchase_price = req.purchase_price;
        let req_currency_code = req.currency_code.clone();
        let req_drink_from_year = req.drink_from_year;
        let req_drink_to_year = req.drink_to_year;
        let req_notes = req.notes.clone();
        let req_rating = req.rating;
        let req_photo_url = req.photo_url.clone();
        let req_clear_grape_varieties = req.clear_grape_varieties;

        let proto_bottle = self
            .db
            .execute_async(move |conn| {
                run_transaction(conn, |tx| {
                    let existing = fetch_bottle_detail(tx, &bottle_uuid)?
                        .ok_or_else(|| ServiceError::NotFound("Bottle not found".to_string()))?;

                    if existing.deleted_at.is_some() {
                        return Err(ServiceError::NotFound("Bottle not found".to_string()));
                    }

                    let name = req_name.or(existing.name);
                    let producer = req_producer.or(existing.producer);
                    let grape_variety = if req_clear_grape_varieties == Some(true) {
                        Vec::new()
                    } else if req_grape_variety.is_empty() {
                        existing.grape_variety
                    } else {
                        req_grape_variety.clone()
                    };
                    let vintage = req_vintage.or(existing.vintage);
                    let country = req_country.or(existing.country);
                    let region = req_region.or(existing.region);
                    let color = req_color
                        .map(ModelWineColor::from)
                        .unwrap_or(existing.color);
                    let quantity = req_quantity.or(existing.quantity);
                    let purchase_date = req_purchase_date
                        .as_ref()
                        .map(|s| parse_naive_date(s))
                        .transpose()?
                        .or(existing.purchase_date);
                    let purchase_price = req_purchase_price.or(existing.purchase_price);
                    let currency_code = req_currency_code.or(existing.currency_code);
                    let drink_from_year = req_drink_from_year.or(existing.drink_from_year);
                    let drink_to_year = req_drink_to_year.or(existing.drink_to_year);
                    let notes = req_notes.or(existing.notes);
                    let rating = req_rating.or(existing.rating);
                    let photo_url = req_photo_url.or(existing.photo_url);

                    tx.execute(
                        "UPDATE wine_bottles SET name = ?1, producer = ?2, grape_variety = ?3, vintage = ?4,
                         country = ?5, region = ?6, color = ?7, quantity = ?8, purchase_date = ?9,
                         purchase_price = ?10, currency_code = ?11, drink_from_year = ?12, drink_to_year = ?13,
                         notes = ?14, rating = ?15, photo_url = ?16, updated_at = ?17 WHERE id = ?18",
                        params![
                            name,
                            producer,
                            ModelWineBottle::grape_variety_to_string(&grape_variety),
                            vintage,
                            country,
                            region,
                            i32::from(color),
                            quantity,
                            purchase_date.map(|d| d.to_string()),
                            purchase_price,
                            currency_code,
                            drink_from_year,
                            drink_to_year,
                            notes,
                            rating,
                            photo_url,
                            now.to_rfc3339(),
                            bottle_uuid.to_string(),
                        ],
                    )
                    .map_err(ServiceError::from)?;

                    set_bottle_varieties(tx, &bottle_uuid, &grape_variety)?;

                    let updated = fetch_bottle_detail(tx, &bottle_uuid)?
                        .ok_or_else(|| ServiceError::NotFound("Bottle not found".to_string()))?;

                    Ok(updated)
                })
            })
            .await?;

        Ok(Response::new(UpdateWineBottleResponse {
            bottle: Some(bottle_to_proto(&proto_bottle)),
        }))
    }

    async fn delete_wine_bottle(
        &self,
        request: Request<DeleteWineBottleRequest>,
    ) -> Result<Response<DeleteWineBottleResponse>, Status> {
        let bottle_uuid = Uuid::parse_str(&request.into_inner().id)
            .map_err(|_| Status::invalid_argument("Invalid bottle ID"))?;

        let now = chrono::Utc::now();
        let now_str = now.to_rfc3339();
        let bottle_id_str = bottle_uuid.to_string();
        let rows = self
            .db
            .execute(move |conn| {
                conn.execute(
                    "UPDATE wine_bottles SET deleted_at = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
                    params![now_str, now_str, bottle_id_str],
                )
            })
            .await?;

        if rows == 0 {
            return Err(Status::not_found("Bottle not found or already deleted"));
        }

        Ok(Response::new(DeleteWineBottleResponse { success: true }))
    }

    async fn list_wine_bottle(
        &self,
        request: Request<ListWineBottleRequest>,
    ) -> Result<Response<ListWineBottleResponse>, Status> {
        let req = request.into_inner();

        let filter = req.filter.unwrap_or_default();
        let pagination = req
            .pagination
            .unwrap_or(crate::generated::cellar::PaginationParams {
                limit: 50,
                offset: 0,
                cursor: None,
            });

        let result = self
            .db
            .execute_async(move |conn| {
                let mut conditions = Vec::new();
                let mut count_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

                conditions.push("deleted_at IS NULL".to_string());

                if let Some(color) = filter.color {
                    conditions.push("color = ?".to_string());
                    count_params.push(Box::new(color));
                }

                if let Some(ref country) = filter.country {
                    conditions.push("country = ?".to_string());
                    count_params.push(Box::new(country.clone()));
                }

                if let Some(ref region) = filter.region {
                    conditions.push("region = ?".to_string());
                    count_params.push(Box::new(region.clone()));
                }

                if let Some(ref region_contains) = filter.region_contains {
                    conditions.push("region LIKE ?".to_string());
                    count_params.push(Box::new(format!("%{}%", region_contains)));
                }

                if let Some(ref producer) = filter.producer {
                    conditions.push("producer = ?".to_string());
                    count_params.push(Box::new(producer.clone()));
                }

                if let Some(ref producer_contains) = filter.producer_contains {
                    conditions.push("producer LIKE ?".to_string());
                    count_params.push(Box::new(format!("%{}%", producer_contains)));
                }

                if let Some(vintage_eq) = filter.vintage_eq {
                    conditions.push("vintage = ?".to_string());
                    count_params.push(Box::new(vintage_eq));
                }

                if let Some(ref range) = filter.vintage_range {
                    conditions.push("vintage >= ? AND vintage <= ?".to_string());
                    count_params.push(Box::new(range.min));
                    count_params.push(Box::new(range.max));
                }

                if let Some(ref range) = filter.rating_range {
                    conditions.push("rating >= ? AND rating <= ?".to_string());
                    count_params.push(Box::new(range.min));
                    count_params.push(Box::new(range.max));
                }

                if let Some(ref name_contains) = filter.name_contains {
                    conditions.push("name LIKE ?".to_string());
                    count_params.push(Box::new(format!("%{}%", name_contains)));
                }

                if let Some(ref grape_variety_filter) = filter.grape_variety {
                    conditions.push(
                        "EXISTS (SELECT 1 FROM wine_bottle_varieties wbv \
                         JOIN grape_varieties gv ON wbv.variety_id = gv.id \
                         WHERE wbv.bottle_id = wine_bottles.id AND gv.name LIKE ?)"
                            .to_string(),
                    );
                    count_params.push(Box::new(format!("%{}%", grape_variety_filter)));
                }

                if filter.drinkable_now == Some(true) {
                    let current_year = chrono::Utc::now().date_naive().year();
                    conditions.push("drink_from_year <= ? AND drink_to_year >= ?".to_string());
                    count_params.push(Box::new(current_year));
                    count_params.push(Box::new(current_year));
                }

                if let Some(ref range) = filter.drink_window_overlap {
                    conditions.push("drink_from_year <= ? AND drink_to_year >= ?".to_string());
                    count_params.push(Box::new(range.min));
                    count_params.push(Box::new(range.max));
                }

                if let Some(ref range) = filter.quantity_range {
                    conditions.push("quantity >= ? AND quantity <= ?".to_string());
                    count_params.push(Box::new(range.min));
                    count_params.push(Box::new(range.max));
                }

                let sort_column = get_sort_column(filter.sort_by);
                let order_dir = if filter.ascending { "ASC" } else { "DESC" };
                let order_by = format!("{} {}", sort_column, order_dir);

                let where_clause = conditions.join(" AND ");
                let count_sql = format!("SELECT COUNT(*) FROM wine_bottles WHERE {}", where_clause);

                let count_params_refs: Vec<&dyn rusqlite::ToSql> =
                    count_params.iter().map(|p| p.as_ref()).collect();
                let total_count: i32 = conn
                    .query_row(&count_sql, count_params_refs.as_slice(), |row| row.get(0))
                    .map_err(|e| {
                        ServiceError::Internal(format!("Failed to count bottles: {}", e))
                    })?;

                let limit = pagination.limit.clamp(1, 100);
                let offset = pagination.offset.max(0);

                let result_sql = format!(
                    "SELECT id, name, producer, vintage, country, region, color
                     FROM wine_bottles WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
                    where_clause, order_by
                );

                let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = count_params;
                all_params.push(Box::new(limit));
                all_params.push(Box::new(offset));

                let all_params_refs: Vec<&dyn rusqlite::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();

                let mut stmt = conn
                    .prepare(&result_sql)
                    .map_err(|e| ServiceError::Internal(format!("Failed to prepare: {}", e)))?;

                let mut rows = stmt.query(all_params_refs.as_slice()).map_err(|e| {
                    ServiceError::Internal(format!("Failed to query bottles: {}", e))
                })?;

                let mut bottles = Vec::new();
                while let Some(row) = rows
                    .next()
                    .map_err(|e| ServiceError::Internal(format!("Failed to read row: {}", e)))?
                {
                    let bottle_id_str: String = row
                        .get(0)
                        .map_err(|e| ServiceError::Internal(format!("Failed to get id: {}", e)))?;
                    let bottle_uuid = Uuid::parse_str(&bottle_id_str).map_err(|e| {
                        ServiceError::Internal(format!("Invalid bottle UUID: {}", e))
                    })?;
                    let varieties = get_varieties_for_bottle(conn, &bottle_uuid).map_err(|e| {
                        ServiceError::Internal(format!("Failed to get varieties: {}", e))
                    })?;

                    bottles.push(WineBottleSummary {
                        id: bottle_id_str,
                        name: row
                            .get::<_, Option<String>>(1)
                            .map_err(|e| {
                                ServiceError::Internal(format!("Failed to get name: {}", e))
                            })?
                            .unwrap_or_default(),
                        producer: row
                            .get::<_, Option<String>>(2)
                            .map_err(|e| {
                                ServiceError::Internal(format!("Failed to get producer: {}", e))
                            })?
                            .unwrap_or_default(),
                        grape_variety: varieties,
                        vintage: row
                            .get::<_, Option<i32>>(3)
                            .map_err(|e| {
                                ServiceError::Internal(format!("Failed to get vintage: {}", e))
                            })?
                            .unwrap_or(0),
                        country: row
                            .get::<_, Option<String>>(4)
                            .map_err(|e| {
                                ServiceError::Internal(format!("Failed to get country: {}", e))
                            })?
                            .unwrap_or_default(),
                        region: row
                            .get::<_, Option<String>>(5)
                            .map_err(|e| {
                                ServiceError::Internal(format!("Failed to get region: {}", e))
                            })?
                            .unwrap_or_default(),
                        color: row.get::<_, i32>(6).map_err(|e| {
                            ServiceError::Internal(format!("Failed to get color: {}", e))
                        })?,
                    });
                }

                let next_cursor = if bottles.len() as i32 == limit && pagination.cursor.is_none() {
                    bottles.last().map(|b| format!("{}|{}", b.name, b.id))
                } else {
                    None
                };

                Ok(ListWineBottleResponse {
                    bottles,
                    total_count,
                    next_cursor,
                })
            })
            .await?;

        Ok(Response::new(result))
    }
}
