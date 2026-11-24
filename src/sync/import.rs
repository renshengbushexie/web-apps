use crate::{
    feishu::{Feishu, feishu_json},
    mapping::{self, Table, identity, product_ids, valid_id},
    state::State,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    shop: String,
    id: String,
    scope: String,
    product_ids: Value,
    question: String,
    answer: String,
    source: Option<String>,
    sku: Option<String>,
    model: Option<String>,
}

fn parse(path: &Path) -> Result<Vec<Entry>> {
    ensure!(
        path.metadata()?.len() <= 64 * 1024 * 1024,
        "import file exceeds 64 MiB"
    );
    let raw = std::fs::read_to_string(path).context("expected a UTF-8 CSV or JSONL file")?;
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let mut entries = Vec::new();
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => {
            let mut rdr = csv::Reader::from_reader(raw.as_bytes());
            let names = rdr.headers()?.clone();
            let distinct: BTreeSet<_> = names.iter().collect();
            ensure!(distinct.len() == names.len(), "duplicate CSV column header");
            for rec in rdr.records() {
                let rec = rec?;
                let values = names
                    .iter()
                    .zip(rec.iter())
                    .map(|(k, v)| (k.to_owned(), Value::String(v.to_owned())));
                entries.push(serde_json::from_value(Value::Object(values.collect()))?);
            }
        }
        "jsonl" | "ndjson" => {
            for (n, line) in raw.lines().enumerate() {
                if !line.trim().is_empty() {
                    entries.push(
                        serde_json::from_str(line)
                            .with_context(|| format!("invalid JSONL row {}", n + 1))?,
                    );
                }
            }
        }
        _ => bail!("supported import formats: CSV and JSONL"),
    }
    ensure!(!entries.is_empty(), "empty import file");
    Ok(entries)
}

fn raw_fields(
    t: &Table,
    schema: &BTreeMap<String, u64>,
    entry: &Entry,
) -> Result<Map<String, Value>> {
    let f = &t.fields;
    ensure!(
        valid_id(&entry.shop) && valid_id(&entry.id),
        "invalid shop or source ID"
    );
    ensure!(
        !entry.question.trim().is_empty() && !entry.answer.trim().is_empty(),
        "question and answer required"
    );
    let ids = product_ids(Some(&entry.product_ids))?;
    let label = match entry.scope.as_str() {
        "product" if !ids.is_empty() => &t.product_values[0],
        "shop" if ids.is_empty() => &t.shop_values[0],
        _ => bail!("invalid scope/product_ids combination"),
    };
    let mut out = Map::new();
    out.insert(f.shop.clone(), json!(entry.shop));
    out.insert(f.id.clone(), json!(entry.id));
    out.insert(f.scope.clone(), json!(label));
    out.insert(
        f.product_ids.clone(),
        match schema[&f.product_ids] {
            4 => json!(ids),
            _ => json!(serde_json::to_string(&ids)?),
        },
    );
    for (name, value) in [
        (&f.question, Some(&entry.question)),
        (&f.answer, Some(&entry.answer)),
        (&f.sku, entry.sku.as_ref()),
        (&f.model, entry.model.as_ref()),
        (&f.source, entry.source.as_ref()),
    ] {
        if let Some(value) = value {
            let name = name
                .as_ref()
                .context("import contains a raw field without a column mapping")?;
            let val = if value.is_empty() {
                Value::Null
            } else if schema[name] == 15 {
                ensure!(
                    Some(name) == f.source.as_ref(),
                    "only source may map to a hyperlink column"
                );
                let url = reqwest::Url::parse(value).context("invalid source link")?;
                ensure!(
                    matches!(url.scheme(), "https" | "http"),
                    "unsupported source link"
                );
                json!({"link":value,"text":value})
            } else {
                ensure!(
                    matches!(schema[name], 1 | 3),
                    "raw content requires text or single-select column"
                );
                json!(value)
            };
            out.insert(name.clone(), val);
        }
    }
    Ok(out)
}

fn equivalent(old: Option<&Value>, new: &Value) -> bool {
    if old == Some(new) {
        return true;
    }
    if new.is_null() {
        return old.is_none_or(|v| {
            v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(Vec::is_empty)
        });
    }
    if let Some(s) = new.as_str() {
        return mapping::text(old).is_ok_and(|v| v == s);
    }
    if new.get("link").is_some() {
        return old.and_then(|v| v.get("link")) == new.get("link");
    }
    if new.is_array() && old.is_some_and(Value::is_array) {
        return product_ids(old)
            .ok()
            .zip(product_ids(Some(new)).ok())
            .is_some_and(|(a, b)| a == b);
    }
    false
}
pub fn run(feishu: &Feishu, t: &Table, file: &str, state: &State) -> Result<()> {
    ensure!(t.kind == "answer", "Xiaoduo files require an answer table");
    let rows = parse(Path::new(file))?;
    let token = feishu.token()?;
    let schema = feishu.schema(t, &token)?;
    let mut prepared = BTreeMap::new();
    for row in rows {
        let fields = raw_fields(t, &schema, &row)?;
        ensure!(
            prepared.insert((row.shop, row.id), fields).is_none(),
            "duplicate (shop, source ID) in file"
        );
    }
    let mut existing = BTreeMap::new();
    for row in feishu.list_records(t, &token)? {
        if let Some(key) = identity(&row, &t.fields)? {
            ensure!(
                existing.insert(key, row).is_none(),
                "duplicate (shop, source ID) in Feishu"
            );
        }
    }
    let source = feishu.source_key(t);
    let url = feishu.url(t, "records");
    let mut writes = 0;
    for ((shop, id), mut fields) in prepared {
        let req = if let Some(row) = existing.get(&(shop.clone(), id.clone())) {
            state.clear_token(&source, &shop, &id)?;
            fields.remove(&t.fields.shop);
            fields.remove(&t.fields.id);
            fields.retain(|key, new| !equivalent(row.fields.get(key), new));
            if fields.is_empty() {
                continue;
            }
            feishu.client.put(format!("{}/{}", url, row.record_id))
        } else {
            feishu
                .client
                .post(&url)
                .query(&[("client_token", state.create_token(&source, &shop, &id)?)])
        };
        feishu_json(req.bearer_auth(&token).json(&json!({"fields":fields})))?;
        state.clear_token(&source, &shop, &id)?;
        writes += 1;
    }
    println!("{}: {writes} records created or updated", t.name);
    Ok(())
}
