use super::{ApiError, ApiResult, App, INTERNAL, INVALID};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use serde_json::{Value, json};

fn connect() -> ApiResult<Connection> {
    let path = std::env::var_os("KB_CATALOG_DB")
        .unwrap_or_else(|| "/var/lib/rust-knowledge/catalog/current/catalog.sqlite".into());
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "catalog_unavailable"))
}

fn array(s: String) -> rusqlite::Result<Value> {
    serde_json::from_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub(super) async fn tables(State(app): State<App>) -> ApiResult<Json<Value>> {
    app.read(|_| {
        let db = connect()?;
        let mut stmt = db.prepare("SELECT s.name,s.headers_json,COUNT(r.row_id) FROM sheets s LEFT JOIN rows r ON r.sheet_id=s.sheet_id GROUP BY s.sheet_id ORDER BY s.ordinal").map_err(|_|INTERNAL)?;
        let rows = stmt.query_map([],|r| Ok(json!({"table":r.get::<_,String>(0)?,"columns":array(r.get(1)?)?,"records":r.get::<_,i64>(2)?})))
            .and_then(|rs| rs.collect::<rusqlite::Result<Vec<Value>>>()).map_err(|_|INTERNAL)?;
        Ok(Json(json!({"tables":rows})))
    }).await?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filter {
    table: Option<String>,
    q: Option<String>,
    after: Option<i64>,
    limit: Option<usize>,
}

pub(super) async fn rows(
    State(app): State<App>,
    Query(filter): Query<Filter>,
) -> ApiResult<Json<Value>> {
    let q = filter.q.unwrap_or_default();
    let table = filter.table.unwrap_or_default();
    let after = filter.after.unwrap_or(0);
    let limit = filter.limit.unwrap_or(10);
    if (q.is_empty() && table.is_empty())
        || q.len() > 256
        || table.len() > 256
        || q.chars().chain(table.chars()).any(char::is_control)
        || after < 0
        || !(1..=25).contains(&limit)
    {
        return Err(INVALID);
    }
    app.read(move |_| find(&connect()?, &table, &q, after, limit))
        .await?
}

fn find(db: &Connection, table: &str, q: &str, after: i64, limit: usize) -> ApiResult<Json<Value>> {
    let indexed = q.chars().count() >= 3;
    let (join, condition, needle) = if indexed {
        (
            "JOIN row_fts ON row_fts.rowid=r.row_id",
            "row_fts MATCH ?1",
            format!("\"{}\"", q.replace('"', "\"\"")),
        )
    } else {
        ("", "(?1='' OR instr(r.values_json,?1)>0)", q.to_owned())
    };
    let sql = format!(
        "SELECT r.row_id,s.name,r.row_num,r.values_json,r.is_disabled FROM rows r JOIN sheets s ON s.sheet_id=r.sheet_id {join} WHERE {condition} AND (?2='' OR s.name=?2) AND r.row_id>?3 ORDER BY r.row_id LIMIT ?4"
    );
    let mut stmt = db.prepare(&sql).map_err(|_| INTERNAL)?;
    let fetch = i64::try_from(limit + 1).map_err(|_| INVALID)?;
    let mut hits = stmt.query_map(params![needle,table,after,fetch],|r| Ok(json!({"id":r.get::<_,i64>(0)?,"table":r.get::<_,String>(1)?,"row":r.get::<_,i64>(2)?,"values":array(r.get(3)?)?,"disabled":r.get::<_,Option<bool>>(4)?})))
        .and_then(|rs|rs.collect::<rusqlite::Result<Vec<Value>>>()).map_err(|_| INTERNAL)?;
    let next = if hits.len() > limit {
        hits.truncate(limit);
        hits.last().and_then(|v| v["id"].as_i64())
    } else {
        None
    };
    Ok(Json(json!({"rows":hits,"next_after":next})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_search_respects_sheet_and_cursor() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(r#"
            CREATE TABLE sheets(sheet_id INTEGER, name TEXT);
            CREATE TABLE rows(row_id INTEGER, sheet_id INTEGER, row_num INTEGER, values_json TEXT, is_disabled INTEGER);
            CREATE VIRTUAL TABLE row_fts USING fts5(content,tokenize='trigram');
            INSERT INTO sheets VALUES(1,'SKU'),(2,'QA');
            INSERT INTO rows VALUES(1,1,2,'["A123"]',0),(2,1,3,'["B123"]',1),(3,2,2,'["C123"]',NULL);
            INSERT INTO row_fts(rowid,content) VALUES(1,'A123'),(2,'B123'),(3,'C123');
        "#).unwrap();
        let page = find(&db, "SKU", "123", 0, 1).unwrap().0;
        assert_eq!(page["rows"][0]["values"][0], "A123");
        assert_eq!(page["next_after"], 1);
        let next = find(&db, "SKU", "23", 1, 5).unwrap().0;
        assert_eq!(next["rows"].as_array().unwrap().len(), 1);
        assert_eq!(next["rows"][0]["disabled"], true);
    }
}
