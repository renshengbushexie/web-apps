use crate::mapping::{Record, Table};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

#[cfg(test)]
#[path = "feishu_tests.rs"]
mod tests;

pub struct Feishu {
    pub client: Client,
    pub base: String,
    app_id: String,
    secret: String,
    origin: String,
}
pub fn json_response(req: RequestBuilder) -> Result<Value> {
    let res = req
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(reqwest::Error::without_url)?;
    let mut bytes = Vec::new();
    res.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 32 * 1024 * 1024,
        "response exceeds size limit"
    );
    serde_json::from_slice(&bytes).context("invalid JSON response")
}
pub fn feishu_json(req: RequestBuilder) -> Result<Value> {
    let body = json_response(req)?;
    let code = body
        .get("code")
        .and_then(Value::as_i64)
        .context("missing Feishu response code")?;
    ensure!(code == 0, "Feishu API code {code}");
    Ok(body)
}
#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    has_more: bool,
    page_token: Option<String>,
    total: Option<usize>,
}
pub fn base_token() -> Result<String> {
    let base = crate::required("FEISHU_BASE_TOKEN")?;
    ensure!(
        base.bytes().all(|b| b.is_ascii_alphanumeric()),
        "invalid FEISHU_BASE_TOKEN"
    );
    Ok(base)
}
pub fn source_key(base: &str, table: &Table) -> String {
    format!("{}/{}", base, table.table_id)
}
impl Feishu {
    pub fn new(client: Client) -> Result<Self> {
        Ok(Self {
            client,
            base: base_token()?,
            app_id: crate::required("FEISHU_APP_ID")?,
            secret: crate::required("FEISHU_APP_SECRET")?,
            origin: "https://open.feishu.cn/open-apis".into(),
        })
    }
    pub fn token(&self) -> Result<String> {
        let body = feishu_json(
            self.client
                .post(format!(
                    "{}/auth/v3/tenant_access_token/internal",
                    self.origin
                ))
                .json(&json!({"app_id":self.app_id,"app_secret":self.secret})),
        )?;
        Ok(body["tenant_access_token"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("Feishu returned no tenant token")?
            .to_owned())
    }
    pub fn url(&self, table: &Table, suffix: &str) -> String {
        format!(
            "{}/bitable/v1/apps/{}/tables/{}/{}",
            self.origin, self.base, table.table_id, suffix
        )
    }
    pub fn source_key(&self, table: &Table) -> String {
        source_key(&self.base, table)
    }
    fn list<T: DeserializeOwned>(
        &self,
        table: &Table,
        token: &str,
        suffix: &str,
        size: &str,
    ) -> Result<Vec<T>> {
        let url = self.url(table, suffix);
        let mut cursor = String::new();
        let mut visited = BTreeSet::new();
        let mut out = Vec::new();
        let mut reported = None;
        loop {
            let mut req = self
                .client
                .get(&url)
                .bearer_auth(token)
                .query(&[("page_size", size)]);
            if !cursor.is_empty() {
                req = req.query(&[("page_token", &cursor)]);
            }
            let mut body = feishu_json(req)?;
            let page: Page<T> =
                serde_json::from_value(body["data"].take()).context("invalid Feishu list page")?;
            if let Some(n) = page.total {
                ensure!(
                    reported.is_none_or(|old| old == n),
                    "record count changed during pagination; retry"
                );
                reported = Some(n);
            }
            let count = page.items.len();
            out.extend(page.items);
            ensure!(out.len() <= 1_000_000, "table exceeds scan limit");
            if !page.has_more {
                break;
            }
            cursor = page
                .page_token
                .filter(|s| !s.is_empty())
                .context("missing continuation token")?;
            ensure!(
                count > 0 && visited.insert(cursor.clone()),
                "pagination failed to advance"
            );
        }
        ensure!(
            reported.is_none_or(|n| n == out.len()),
            "incomplete Feishu list"
        );
        Ok(out)
    }
    pub fn schema(&self, table: &Table, token: &str) -> Result<BTreeMap<String, u64>> {
        #[derive(Deserialize)]
        struct Field {
            field_name: String,
            #[serde(rename = "type")]
            kind: u64,
        }
        let fields: Vec<Field> = self.list(table, token, "fields", "100")?;
        let schema: BTreeMap<_, _> = fields.into_iter().map(|f| (f.field_name, f.kind)).collect();
        for name in table.fields.names() {
            ensure!(schema.contains_key(name), "missing mapped column {name}");
        }
        let f = &table.fields;
        ensure!(
            matches!(schema[&f.id], 1 | 3) && matches!(schema[&f.shop], 1 | 3),
            "shop and stable IDs require Feishu text or single-select columns"
        );
        ensure!(
            matches!(schema[&f.product_ids], 1 | 4),
            "product associations require text (JSON IDs) or multi-select"
        );
        ensure!(
            matches!(schema[&f.enabled], 1 | 3 | 7) && matches!(schema[&f.scope], 1 | 3),
            "unexpected status or scope column type"
        );
        Ok(schema)
    }
    pub fn list_records(&self, table: &Table, token: &str) -> Result<Vec<Record>> {
        let rows: Vec<Record> = self.list(table, token, "records", "500")?;
        let mut seen = BTreeSet::new();
        ensure!(
            rows.iter().all(|r| !r.record_id.is_empty()
                && r.record_id.bytes().all(|b| b.is_ascii_alphanumeric())
                && seen.insert(&r.record_id)),
            "missing, invalid or duplicate Feishu record ID"
        );
        Ok(rows)
    }
    pub fn records(&self, table: &Table, token: &str) -> Result<Vec<Record>> {
        self.schema(table, token)?;
        self.list_records(table, token)
    }
}
