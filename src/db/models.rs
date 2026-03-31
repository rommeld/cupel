use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WineBottle {
    pub id: Uuid,
    pub name: Option<String>,
    pub producer: Option<String>,
    pub grape_variety: Vec<String>,
    pub vintage: Option<i32>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub color: WineColor,
    pub quantity: Option<i32>,
    pub purchase_date: Option<NaiveDate>,
    pub purchase_price: Option<f64>,
    pub currency_code: Option<String>,
    pub drink_from_year: Option<i32>,
    pub drink_to_year: Option<i32>,
    pub notes: Option<String>,
    pub rating: Option<i32>,
    pub photo_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum WineColor {
    Unspecified = 0,
    White = 1,
    Red = 2,
    Rose = 3,
    Sparkling = 4,
    Dessert = 5,
    Fortified = 6,
}

impl From<i32> for WineColor {
    fn from(value: i32) -> Self {
        match value {
            1 => WineColor::White,
            2 => WineColor::Red,
            3 => WineColor::Rose,
            4 => WineColor::Sparkling,
            5 => WineColor::Dessert,
            6 => WineColor::Fortified,
            _ => WineColor::Unspecified,
        }
    }
}

impl From<WineColor> for i32 {
    fn from(color: WineColor) -> Self {
        color as i32
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WineCellar {
    pub id: Uuid,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WineCellarBottle {
    pub cellar_id: Uuid,
    pub bottle_id: Uuid,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum DeleteReason {
    Unspecified = 0,
    Drunk = 1,
    Sold = 2,
    Gifted = 3,
    Corked = 4,
    Other = 5,
}

impl From<i32> for DeleteReason {
    fn from(value: i32) -> Self {
        match value {
            1 => DeleteReason::Drunk,
            2 => DeleteReason::Sold,
            3 => DeleteReason::Gifted,
            4 => DeleteReason::Corked,
            5 => DeleteReason::Other,
            _ => DeleteReason::Unspecified,
        }
    }
}

impl From<DeleteReason> for i32 {
    fn from(reason: DeleteReason) -> Self {
        reason as i32
    }
}

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS wine_bottles (
    id TEXT PRIMARY KEY,
    name TEXT,
    producer TEXT,
    grape_variety TEXT,
    vintage INTEGER,
    country TEXT,
    region TEXT,
    color INTEGER NOT NULL DEFAULT 0,
    quantity INTEGER,
    purchase_date TEXT,
    purchase_price REAL,
    currency_code TEXT,
    drink_from_year INTEGER,
    drink_to_year INTEGER,
    notes TEXT,
    rating INTEGER,
    photo_url TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT
);

CREATE TABLE IF NOT EXISTS wine_cellars (
    id TEXT PRIMARY KEY,
    name TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS wine_cellar_bottles (
    cellar_id TEXT NOT NULL,
    bottle_id TEXT NOT NULL,
    added_at TEXT NOT NULL,
    PRIMARY KEY (cellar_id, bottle_id),
    FOREIGN KEY (cellar_id) REFERENCES wine_cellars(id),
    FOREIGN KEY (bottle_id) REFERENCES wine_bottles(id)
);

CREATE INDEX IF NOT EXISTS idx_bottles_country ON wine_bottles(country);
CREATE INDEX IF NOT EXISTS idx_bottles_region ON wine_bottles(region);
CREATE INDEX IF NOT EXISTS idx_bottles_vintage ON wine_bottles(vintage);
CREATE INDEX IF NOT EXISTS idx_bottles_color ON wine_bottles(color);
CREATE INDEX IF NOT EXISTS idx_bottles_deleted_at ON wine_bottles(deleted_at);
CREATE INDEX IF NOT EXISTS idx_cellar_bottles_cellar ON wine_cellar_bottles(cellar_id);
CREATE INDEX IF NOT EXISTS idx_cellar_bottles_bottle ON wine_cellar_bottles(bottle_id);
"#;

pub const GRAPE_VARIETY_SEPARATOR: char = ',';

impl WineBottle {
    pub fn grape_variety_to_string(varieties: &[String]) -> String {
        varieties.join(&GRAPE_VARIETY_SEPARATOR.to_string())
    }

    pub fn grape_variety_from_string(s: &str) -> Vec<String> {
        if s.is_empty() {
            Vec::new()
        } else {
            s.split(GRAPE_VARIETY_SEPARATOR)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wine_color_from_i32() {
        assert_eq!(WineColor::from(0), WineColor::Unspecified);
        assert_eq!(WineColor::from(1), WineColor::White);
        assert_eq!(WineColor::from(2), WineColor::Red);
        assert_eq!(WineColor::from(3), WineColor::Rose);
        assert_eq!(WineColor::from(4), WineColor::Sparkling);
        assert_eq!(WineColor::from(5), WineColor::Dessert);
        assert_eq!(WineColor::from(6), WineColor::Fortified);
    }

    #[test]
    fn test_wine_color_from_i32_invalid() {
        assert_eq!(WineColor::from(99), WineColor::Unspecified);
        assert_eq!(WineColor::from(-1), WineColor::Unspecified);
    }

    #[test]
    fn test_wine_color_to_i32() {
        assert_eq!(i32::from(WineColor::Unspecified), 0);
        assert_eq!(i32::from(WineColor::White), 1);
        assert_eq!(i32::from(WineColor::Red), 2);
        assert_eq!(i32::from(WineColor::Rose), 3);
        assert_eq!(i32::from(WineColor::Sparkling), 4);
        assert_eq!(i32::from(WineColor::Dessert), 5);
        assert_eq!(i32::from(WineColor::Fortified), 6);
    }

    #[test]
    fn test_delete_reason_from_i32() {
        assert_eq!(DeleteReason::from(0), DeleteReason::Unspecified);
        assert_eq!(DeleteReason::from(1), DeleteReason::Drunk);
        assert_eq!(DeleteReason::from(2), DeleteReason::Sold);
        assert_eq!(DeleteReason::from(3), DeleteReason::Gifted);
        assert_eq!(DeleteReason::from(4), DeleteReason::Corked);
        assert_eq!(DeleteReason::from(5), DeleteReason::Other);
    }

    #[test]
    fn test_delete_reason_from_i32_invalid() {
        assert_eq!(DeleteReason::from(99), DeleteReason::Unspecified);
        assert_eq!(DeleteReason::from(-1), DeleteReason::Unspecified);
    }

    #[test]
    fn test_delete_reason_to_i32() {
        assert_eq!(i32::from(DeleteReason::Unspecified), 0);
        assert_eq!(i32::from(DeleteReason::Drunk), 1);
        assert_eq!(i32::from(DeleteReason::Sold), 2);
        assert_eq!(i32::from(DeleteReason::Gifted), 3);
        assert_eq!(i32::from(DeleteReason::Corked), 4);
        assert_eq!(i32::from(DeleteReason::Other), 5);
    }

    #[test]
    fn test_grape_variety_to_string_single() {
        let varieties = vec!["Chardonnay".to_string()];
        assert_eq!(
            WineBottle::grape_variety_to_string(&varieties),
            "Chardonnay"
        );
    }

    #[test]
    fn test_grape_variety_to_string_multiple() {
        let varieties = vec!["Chardonnay".to_string(), "Pinot Noir".to_string()];
        assert_eq!(
            WineBottle::grape_variety_to_string(&varieties),
            "Chardonnay,Pinot Noir"
        );
    }

    #[test]
    fn test_grape_variety_to_string_empty() {
        let varieties: Vec<String> = vec![];
        assert_eq!(WineBottle::grape_variety_to_string(&varieties), "");
    }

    #[test]
    fn test_grape_variety_from_string_single() {
        let result = WineBottle::grape_variety_from_string("Chardonnay");
        assert_eq!(result, vec!["Chardonnay"]);
    }

    #[test]
    fn test_grape_variety_from_string_multiple() {
        let result = WineBottle::grape_variety_from_string("Chardonnay,Pinot Noir");
        assert_eq!(result, vec!["Chardonnay", "Pinot Noir"]);
    }

    #[test]
    fn test_grape_variety_from_string_with_spaces() {
        let result = WineBottle::grape_variety_from_string("Chardonnay, Pinot Noir , Riesling");
        assert_eq!(result, vec!["Chardonnay", "Pinot Noir", "Riesling"]);
    }

    #[test]
    fn test_grape_variety_from_string_empty() {
        assert_eq!(
            WineBottle::grape_variety_from_string(""),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_grape_variety_from_string_only_separator() {
        assert_eq!(
            WineBottle::grape_variety_from_string(","),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_grape_variety_roundtrip() {
        let original = vec!["Cabernet Sauvignon".to_string(), "Merlot".to_string()];
        let serialized = WineBottle::grape_variety_to_string(&original);
        let deserialized = WineBottle::grape_variety_from_string(&serialized);
        assert_eq!(original, deserialized);
    }
}
