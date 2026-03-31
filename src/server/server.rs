use rusqlite::{Connection, OptionalExtension};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::db::DbPool;
use crate::generated::cellar::{
    wine_bottle_service_server::WineBottleService, wine_cellar_service_server::WineCellarService,
    CreateWineBottleRequest, CreateWineBottleResponse, CreateWineCellarRequest,
    CreateWineCellarResponse, DeleteWineBottleRequest, DeleteWineBottleResponse,
    DeleteWineCellarRequest, DeleteWineCellarResponse, GetWineBottleRequest, GetWineBottleResponse,
    ListWineBottleRequest, ListWineBottleResponse, UpdateWineBottleRequest,
    UpdateWineBottleResponse, UpdateWineCellarRequest, UpdateWineCellarResponse, WineBottleDetail,
    WineBottleSummary, WineCellar as ProtoWineCellar,
};
use crate::db::models::{
    WineBottle as ModelWineBottle, WineCellar as ModelWineCellar,
    WineColor as ModelWineColor, GRAPE_VARIETY_SEPARATOR,
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

fn parse_iso(s: &str) -> Result<chrono::DateTime<chrono::Utc>, Status> {
    s.parse::<chrono::DateTime<chrono::Utc>>()
        .map_err(|_| Status::invalid_argument("Invalid ISO date format"))
}

fn parse_naive_date(s: &str) -> Result<chrono::NaiveDate, Status> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| Status::invalid_argument("Invalid date format, expected YYYY-MM-DD"))
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

fn proto_to_bottle(request: &CreateWineBottleRequest) -> Result<ModelWineBottle, Status> {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    let created_at = now;
    let updated_at = now;

    let grape_variety = if request.grape_variety.is_empty() {
        Vec::new()
    } else {
        request.grape_variety.clone()
    };

    let color = request
        .color
        .map(ModelWineColor::from)
        .unwrap_or(ModelWineColor::Unspecified);

    let purchase_date = if let Some(ref date_str) = request.purchase_date {
        if !date_str.is_empty() {
            Some(parse_naive_date(date_str)?)
        } else {
            None
        }
    } else {
        None
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
        created_at,
        updated_at,
        deleted_at: None,
    })
}

#[tonic::async_trait]
impl WineCellarService for AppState {
    async fn create_wine_cellar(
        &self,
        request: Request<CreateWineCellarRequest>,
    ) -> Result<Response<CreateWineCellarResponse>, Status> {
        let req = request.into_inner();
        let db = self.db.0.lock().await;
        let now = chrono::Utc::now();
        let cellar_id = Uuid::new_v4();

        db.execute(
            "INSERT INTO wine_cellars (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![cellar_id.to_string(), req.name, now.to_rfc3339(), now.to_rfc3339()],
        )
        .map_err(|e| Status::internal(format!("Failed to create cellar: {}", e)))?;

        for bottle_req in &req.new_bottles {
            let mut bottle = proto_to_bottle(bottle_req)?;
            bottle.id = Uuid::new_v4();
            
            db.execute(
                "INSERT INTO wine_bottles (id, name, producer, grape_variety, vintage, country, region, color, quantity, purchase_date, purchase_price, currency_code, drink_from_year, drink_to_year, notes, rating, photo_url, created_at, updated_at, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                rusqlite::params![
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
            .map_err(|e| Status::internal(format!("Failed to create bottle: {}", e)))?;

            db.execute(
                "INSERT INTO wine_cellar_bottles (cellar_id, bottle_id, added_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![cellar_id.to_string(), bottle.id.to_string(), now.to_rfc3339()],
            )
            .map_err(|e| Status::internal(format!("Failed to link bottle to cellar: {}", e)))?;
        }

        for bottle_id in &req.existing_bottle_ids {
            db.execute(
                "INSERT INTO wine_cellar_bottles (cellar_id, bottle_id, added_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![cellar_id.to_string(), bottle_id, now.to_rfc3339()],
            )
            .map_err(|e| Status::internal(format!("Failed to link existing bottle: {}", e)))?;
        }

        let bottles = fetch_cellar_bottles(&db, &cellar_id)?;

        let proto_cellar = ProtoWineCellar {
            id: cellar_id.to_string(),
            name: Some(req.name),
            bottles,
        };

        Ok(Response::new(CreateWineCellarResponse {
            wine_cellar: Some(proto_cellar),
        }))
    }

    async fn update_wine_cellar(
        &self,
        request: Request<UpdateWineCellarRequest>,
    ) -> Result<Response<UpdateWineCellarResponse>, Status> {
        let req = request.into_inner();
        let db = self.db.0.lock().await;
        let now = chrono::Utc::now();

        let cellar_uuid = Uuid::parse_str(&req.id)
            .map_err(|_| Status::invalid_argument("Invalid cellar ID"))?;

        if let Some(ref name) = req.name {
            db.execute(
                "UPDATE wine_cellars SET name = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![name, now.to_rfc3339(), cellar_uuid.to_string()],
            )
            .map_err(|e| Status::internal(format!("Failed to update cellar: {}", e)))?;
        }

        if !req.bottle_ids.is_empty() {
            db.execute(
                "DELETE FROM wine_cellar_bottles WHERE cellar_id = ?1",
                rusqlite::params![cellar_uuid.to_string()],
            )
            .map_err(|e| Status::internal(format!("Failed to clear bottles: {}", e)))?;

            for bottle_id in &req.bottle_ids {
                db.execute(
                    "INSERT INTO wine_cellar_bottles (cellar_id, bottle_id, added_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![cellar_uuid.to_string(), bottle_id, now.to_rfc3339()],
                )
                .map_err(|e| Status::internal(format!("Failed to add bottle: {}", e)))?;
            }
        }

        let mut stmt = db
            .prepare("SELECT id, name, created_at, updated_at FROM wine_cellars WHERE id = ?1")
            .map_err(|e| Status::internal(format!("Failed to prepare: {}", e)))?;

        let cellar = stmt
            .query_row(rusqlite::params![cellar_uuid.to_string()], |row| {
                Ok(ModelWineCellar {
                    id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap(),
                    name: row.get(1)?,
                    created_at: parse_iso(&row.get::<_, String>(2)?).unwrap(),
                    updated_at: parse_iso(&row.get::<_, String>(3)?).unwrap(),
                })
            })
            .map_err(|_| Status::not_found("Cellar not found"))?;

        let bottles = fetch_cellar_bottles(&db, &cellar_uuid)?;

        let proto_cellar = ProtoWineCellar {
            id: cellar.id.to_string(),
            name: cellar.name,
            bottles,
        };

        Ok(Response::new(UpdateWineCellarResponse {
            wine_cellar: Some(proto_cellar),
        }))
    }

    async fn delete_wine_cellar(
        &self,
        request: Request<DeleteWineCellarRequest>,
    ) -> Result<Response<DeleteWineCellarResponse>, Status> {
        let req = request.into_inner();
        let db = self.db.0.lock().await;

        let cellar_uuid = Uuid::parse_str(&req.id)
            .map_err(|_| Status::invalid_argument("Invalid cellar ID"))?;

        db.execute(
            "DELETE FROM wine_cellar_bottles WHERE cellar_id = ?1",
            rusqlite::params![cellar_uuid.to_string()],
        )
        .map_err(|e| Status::internal(format!("Failed to delete cellar bottles: {}", e)))?;

        db.execute(
            "DELETE FROM wine_cellars WHERE id = ?1",
            rusqlite::params![cellar_uuid.to_string()],
        )
        .map_err(|e| Status::internal(format!("Failed to delete cellar: {}", e)))?;

        Ok(Response::new(DeleteWineCellarResponse { success: true }))
    }
}

fn fetch_cellar_bottles(db: &Connection, cellar_id: &Uuid) -> Result<Vec<WineBottleSummary>, Status> {
    let mut stmt = db
        .prepare(
            "SELECT b.id, b.name, b.producer, b.grape_variety, b.vintage, b.country, b.region, b.color
             FROM wine_bottles b
             INNER JOIN wine_cellar_bottles cb ON b.id = cb.bottle_id
             WHERE cb.cellar_id = ?1 AND b.deleted_at IS NULL",
        )
        .map_err(|e| Status::internal(format!("Failed to prepare: {}", e)))?;

    let bottles = stmt
        .query_map(rusqlite::params![cellar_id.to_string()], |row: &rusqlite::Row| {
            let grape_str: String = row.get(3)?;
            Ok(WineBottleSummary {
                id: row.get(0)?,
                name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                producer: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                grape_variety: if grape_str.is_empty() {
                    Vec::new()
                } else {
                    grape_str.split(GRAPE_VARIETY_SEPARATOR).map(|s: &str| s.trim().to_string()).collect()
                },
                vintage: row.get::<_, Option<i32>>(4)?.unwrap_or(0),
                country: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                region: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                color: row.get::<_, i32>(7)?,
            })
        })
        .map_err(|e| Status::internal(format!("Failed to query bottles: {}", e)))?;

    let mut result = Vec::new();
    for bottle in bottles {
        result.push(bottle.map_err(|e| Status::internal(format!("Failed to read bottle: {}", e)))?);
    }

    Ok(result)
}

fn fetch_bottle_detail(db: &Connection, bottle_id: &Uuid) -> Result<Option<ModelWineBottle>, Status> {
    let mut stmt = db
        .prepare(
            "SELECT id, name, producer, grape_variety, vintage, country, region, color,
                    quantity, purchase_date, purchase_price, currency_code,
                    drink_from_year, drink_to_year, notes, rating, photo_url,
                    created_at, updated_at, deleted_at
             FROM wine_bottles WHERE id = ?1",
        )
        .map_err(|e| Status::internal(format!("Failed to prepare: {}", e)))?;

    let bottle = stmt
        .query_row(rusqlite::params![bottle_id.to_string()], |row: &rusqlite::Row| {
            let grape_str: String = row.get(3)?;
            let purchase_date_str: Option<String> = row.get(9)?;
            let deleted_at_str: Option<String> = row.get(19)?;

            Ok(ModelWineBottle {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap(),
                name: row.get(1)?,
                producer: row.get(2)?,
                grape_variety: if grape_str.is_empty() {
                    Vec::new()
                } else {
                    grape_str.split(GRAPE_VARIETY_SEPARATOR).map(|s: &str| s.trim().to_string()).collect()
                },
                vintage: row.get(4)?,
                country: row.get(5)?,
                region: row.get(6)?,
                color: ModelWineColor::from(row.get::<_, i32>(7)?),
                quantity: row.get(8)?,
                purchase_date: purchase_date_str.and_then(|s: String| chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()),
                purchase_price: row.get(10)?,
                currency_code: row.get(11)?,
                drink_from_year: row.get(12)?,
                drink_to_year: row.get(13)?,
                notes: row.get(14)?,
                rating: row.get(15)?,
                photo_url: row.get(16)?,
                created_at: parse_iso(&row.get::<_, String>(17)?).unwrap(),
                updated_at: parse_iso(&row.get::<_, String>(18)?).unwrap(),
                deleted_at: deleted_at_str.and_then(|s: String| s.parse::<chrono::DateTime<chrono::Utc>>().ok()),
            })
        })
        .optional()
        .map_err(|e| Status::internal(format!("Failed to query bottle: {}", e)))?;

    Ok(bottle)
}

#[tonic::async_trait]
impl WineBottleService for AppState {
    async fn create_wine_bottle(
        &self,
        request: Request<CreateWineBottleRequest>,
    ) -> Result<Response<CreateWineBottleResponse>, Status> {
        let req = request.into_inner();
        let db = self.db.0.lock().await;

        let bottle = proto_to_bottle(&req)?;

        db.execute(
            "INSERT INTO wine_bottles (id, name, producer, grape_variety, vintage, country, region, color, quantity, purchase_date, purchase_price, currency_code, drink_from_year, drink_to_year, notes, rating, photo_url, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            rusqlite::params![
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
        .map_err(|e| Status::internal(format!("Failed to create bottle: {}", e)))?;

        Ok(Response::new(CreateWineBottleResponse {
            bottle: Some(bottle_to_proto(&bottle)),
        }))
    }

    async fn get_wine_bottle(
        &self,
        request: Request<GetWineBottleRequest>,
    ) -> Result<Response<GetWineBottleResponse>, Status> {
        let req = request.into_inner();
        let db = self.db.0.lock().await;

        let bottle_uuid = Uuid::parse_str(&req.id)
            .map_err(|_| Status::invalid_argument("Invalid bottle ID"))?;

        let bottle = fetch_bottle_detail(&db, &bottle_uuid)?
            .ok_or_else(|| Status::not_found("Bottle not found"))?;

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
        let db = self.db.0.lock().await;
        let now = chrono::Utc::now();

        let bottle_uuid = Uuid::parse_str(&req.id)
            .map_err(|_| Status::invalid_argument("Invalid bottle ID"))?;

        let existing = fetch_bottle_detail(&db, &bottle_uuid)?
            .ok_or_else(|| Status::not_found("Bottle not found"))?;

        if existing.deleted_at.is_some() {
            return Err(Status::not_found("Bottle not found"));
        }

        let name = req.name.or(existing.name);
        let producer = req.producer.or(existing.producer);
        let grape_variety = if req.grape_variety.is_empty() {
            existing.grape_variety
        } else {
            req.grape_variety
        };
        let vintage = req.vintage.or(existing.vintage);
        let country = req.country.or(existing.country);
        let region = req.region.or(existing.region);
        let color = req.color.map(ModelWineColor::from).unwrap_or(existing.color);
        let quantity = req.quantity.or(existing.quantity);
        let purchase_date = req.purchase_date.map(|s| parse_naive_date(&s)).transpose()?.or(existing.purchase_date);
        let purchase_price = req.purchase_price.or(existing.purchase_price);
        let currency_code = req.currency_code.or(existing.currency_code);
        let drink_from_year = req.drink_from_year.or(existing.drink_from_year);
        let drink_to_year = req.drink_to_year.or(existing.drink_to_year);
        let notes = req.notes.or(existing.notes);
        let rating = req.rating.or(existing.rating);
        let photo_url = req.photo_url.or(existing.photo_url);

        db.execute(
            "UPDATE wine_bottles SET name = ?1, producer = ?2, grape_variety = ?3, vintage = ?4,
             country = ?5, region = ?6, color = ?7, quantity = ?8, purchase_date = ?9,
             purchase_price = ?10, currency_code = ?11, drink_from_year = ?12, drink_to_year = ?13,
             notes = ?14, rating = ?15, photo_url = ?16, updated_at = ?17 WHERE id = ?18",
            rusqlite::params![
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
        .map_err(|e| Status::internal(format!("Failed to update bottle: {}", e)))?;

        let updated = fetch_bottle_detail(&db, &bottle_uuid)?
            .ok_or_else(|| Status::not_found("Bottle not found"))?;

        Ok(Response::new(UpdateWineBottleResponse {
            bottle: Some(bottle_to_proto(&updated)),
        }))
    }

    async fn delete_wine_bottle(
        &self,
        request: Request<DeleteWineBottleRequest>,
    ) -> Result<Response<DeleteWineBottleResponse>, Status> {
        let req = request.into_inner();
        let db = self.db.0.lock().await;
        let now = chrono::Utc::now();

        let bottle_uuid = Uuid::parse_str(&req.id)
            .map_err(|_| Status::invalid_argument("Invalid bottle ID"))?;

        db.execute(
            "UPDATE wine_bottles SET deleted_at = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
            rusqlite::params![now.to_rfc3339(), now.to_rfc3339(), bottle_uuid.to_string()],
        )
        .map_err(|e| Status::internal(format!("Failed to delete bottle: {}", e)))?;

        Ok(Response::new(DeleteWineBottleResponse { success: true }))
    }

    async fn list_wine_bottle(
        &self,
        _request: Request<ListWineBottleRequest>,
    ) -> Result<Response<ListWineBottleResponse>, Status> {
        let db = self.db.0.lock().await;

        let mut stmt = db
            .prepare(
                "SELECT id, name, producer, grape_variety, vintage, country, region, color
                 FROM wine_bottles WHERE deleted_at IS NULL",
            )
            .map_err(|e| Status::internal(format!("Failed to prepare: {}", e)))?;

        let bottles = stmt
            .query_map([], |row: &rusqlite::Row| {
                let grape_str: String = row.get(3)?;
                Ok(WineBottleSummary {
                    id: row.get(0)?,
                    name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    producer: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    grape_variety: if grape_str.is_empty() {
                        Vec::new()
                    } else {
                        grape_str.split(GRAPE_VARIETY_SEPARATOR).map(|s: &str| s.trim().to_string()).collect()
                    },
                    vintage: row.get::<_, Option<i32>>(4)?.unwrap_or(0),
                    country: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    region: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    color: row.get::<_, i32>(7)?,
                })
            })
            .map_err(|e| Status::internal(format!("Failed to query bottles: {}", e)))?;

        let mut result = Vec::new();
        let mut total_count = 0i32;
        for bottle in bottles {
            total_count += 1;
            result.push(bottle.map_err(|e| Status::internal(format!("Failed to read bottle: {}", e)))?);
        }

        Ok(Response::new(ListWineBottleResponse {
            bottles: result,
            total_count,
            next_cursor: None,
        }))
    }
}
