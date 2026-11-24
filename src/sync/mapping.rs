use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub type Snapshot = BTreeMap<(String, String), Value>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub name: String,
    pub table_id: String,
    pub kind: String,
    pub fields: Fields,
    #[serde(default = "enabled_values")]
    pub enabled_values: Vec<String>,
    #[serde(default = "product_values")]
    pub product_values: Vec<String>,
    #[serde(default = "shop_values")]
    pub shop_values: Vec<String>,
}
fn enabled_values() -> Vec<String> {
    ["enabled", "true", "启用", "已启用"]
        .map(str::to_owned)
        .into()
}
fn product_values() -> Vec<String> {
    ["product", "商品"].map(str::to_owned).into()
}
fn shop_values() -> Vec<String> {
    ["shop", "店铺"].map(str::to_owned).into()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fields {
    pub shop: String,
    pub id: String,
    pub scope: String,
    pub product_ids: String,
    pub enabled: String,
    pub sku: Option<String>,
    pub model: Option<String>,
    pub question: Option<String>,
    pub answer: Option<String>,
    pub source: Option<String>,
    pub question_override: Option<String>,
    pub answer_override: Option<String>,
}
impl Fields {
    pub fn names(&self) -> Vec<&str> {
        [
            Some(&self.shop),
            Some(&self.id),
            Some(&self.scope),
            Some(&self.product_ids),
            Some(&self.enabled),
            self.sku.as_ref(),
            self.model.as_ref(),
            self.question.as_ref(),
            self.answer.as_ref(),
            self.source.as_ref(),
            self.question_override.as_ref(),
            self.answer_override.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect()
    }
}
pub fn validate_tables(tables: &[Table]) -> Result<()> {
    ensure!(!tables.is_empty(), "KB_TABLES_JSON has no tables");
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for t in tables {
        ensure!(
            !t.name.is_empty() && names.insert(&t.name),
            "empty/duplicate table name"
        );
        ensure!(
            t.table_id.starts_with("tbl")
                && t.table_id.bytes().all(|b| b.is_ascii_alphanumeric())
                && ids.insert(&t.table_id),
            "invalid or duplicate Feishu table ID"
        );
        match t.kind.as_str() {
            "sku" => ensure!(
                t.fields.sku.is_some() || t.fields.model.is_some(),
                "sku table requires sku or model mapping"
            ),
            "answer" => ensure!(
                t.fields.question.is_some() && t.fields.answer.is_some(),
                "answer table requires question and answer mappings"
            ),
            _ => bail!("unknown table kind"),
        }
        let mut distinct = BTreeSet::new();
        ensure!(
            t.fields
                .names()
                .into_iter()
                .all(|s| !s.trim().is_empty() && distinct.insert(s)),
            "each raw, override and control field must map to a distinct nonempty Feishu column"
        );
        ensure!(
            !t.product_values.is_empty()
                && !t.shop_values.is_empty()
                && t.product_values
                    .iter()
                    .chain(&t.shop_values)
                    .chain(&t.enabled_values)
                    .all(|v| !v.trim().is_empty())
                && t.product_values.iter().all(|v| !t.shop_values.contains(v)),
            "invalid scope/status labels"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct Record {
    pub record_id: String,
    pub fields: BTreeMap<String, Value>,
}

pub fn text(value: Option<&Value>) -> Result<String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                out.push_str(
                    p.get("text")
                        .and_then(Value::as_str)
                        .context("expected Feishu text segments")?,
                );
            }
            Ok(out)
        }
        _ => bail!("expected a text field; numeric identifiers cannot preserve leading zeros"),
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id == id.trim() && id.len() <= 2048 && !id.chars().any(char::is_control)
}
pub fn product_ids(value: Option<&Value>) -> Result<Vec<String>> {
    let mut ids = match value {
        Some(Value::Array(parts)) if parts.iter().all(Value::is_string) => parts
            .iter()
            .map(|p| p.as_str().unwrap().to_owned())
            .collect(),
        _ => {
            let s = text(value)?;
            if s.trim().is_empty() {
                vec![]
            } else if s.trim_start().starts_with('[') {
                serde_json::from_str::<Vec<String>>(&s)
                    .context("invalid JSON array of product ID strings")?
            } else {
                vec![s]
            }
        }
    };
    ensure!(
        ids.iter().all(|s| valid_id(s)),
        "invalid product ID in association"
    );
    ids.sort();
    ids.dedup();
    Ok(ids)
}
pub fn identity(row: &Record, f: &Fields) -> Result<Option<(String, String)>> {
    let shop = text(row.fields.get(&f.shop))?;
    let id = text(row.fields.get(&f.id))?;
    if shop.is_empty()
        && id.is_empty()
        && row.fields.values().all(|v| {
            v.is_null()
                || v.as_str() == Some("")
                || v.as_bool() == Some(false)
                || v.as_array().is_some_and(Vec::is_empty)
        })
    {
        return Ok(None);
    }
    ensure!(
        !shop.trim().is_empty() && !id.trim().is_empty(),
        "missing shop or stable source ID"
    );
    ensure!(
        valid_id(&shop) && valid_id(&id),
        "invalid shop or identifier"
    );
    Ok(Some((shop, id)))
}

pub fn snapshot(table: &Table, base: &str, records: &[Record]) -> Result<Snapshot> {
    let mut seen = BTreeSet::new();
    let mut result = Snapshot::new();
    let f = &table.fields;
    for r in records {
        let Some(key) = identity(r, f).with_context(|| format!("record {}", r.record_id))? else {
            continue;
        };
        ensure!(
            seen.insert(key.clone()),
            "duplicate (shop, stable source ID) in {}",
            table.name
        );
        let get = |k: &Option<String>| text(k.as_ref().and_then(|s| r.fields.get(s)));
        let status = r.fields.get(&f.enabled);
        let enabled = match status {
            Some(Value::Bool(b)) => *b,
            _ => text(status).is_ok_and(|s| table.enabled_values.contains(&s)),
        };
        if !enabled {
            continue;
        }
        let scope = text(r.fields.get(&f.scope)).unwrap_or_default();
        let scope = if table.product_values.contains(&scope) {
            "product"
        } else if table.shop_values.contains(&scope) {
            "shop"
        } else {
            continue;
        };
        let product_ids = product_ids(r.fields.get(&f.product_ids))?;
        if (scope == "product" && product_ids.is_empty())
            || (scope == "shop" && (!product_ids.is_empty() || table.kind == "sku"))
        {
            continue;
        }
        let field_text = |raw: &Option<String>, over: &Option<String>| -> Result<String> {
            let edited = get(over)?;
            if edited.trim().is_empty() {
                get(raw)
            } else {
                Ok(edited)
            }
        };
        let sku = get(&f.sku)?;
        let question = field_text(&f.question, &f.question_override)?;
        let answer = field_text(&f.answer, &f.answer_override)?;
        if table.kind == "answer" && (question.trim().is_empty() || answer.trim().is_empty()) {
            continue;
        }
        let src = f.source.as_ref().and_then(|n| r.fields.get(n));
        let source = match src.and_then(|v| v.get("link")).and_then(Value::as_str) {
            Some(url) => url.to_owned(),
            None => text(src)?,
        };
        let source = if source.is_empty() {
            format!(
                "https://www.feishu.cn/base/{base}?table={}&record={}",
                table.table_id, r.record_id
            )
        } else {
            source
        };
        let item = json!({"shop": key.0,"id":key.1,"kind":table.kind,"scope":scope,"product_ids":product_ids,
            "sku":sku,"model":get(&f.model)?,"question":question,"answer":answer,"source":source,"enabled":true});
        result.insert(key, item);
    }
    Ok(result)
}
