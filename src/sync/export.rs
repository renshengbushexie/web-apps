use crate::{
    feishu::{base_token, source_key},
    mapping::{self, Keys, Record, Snapshot, Table},
    state::{Admin, State},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{collections::BTreeSet, env, fs::File, io::Read, path::Path};

const MAX_BYTES: usize = 64 * 1024 * 1024;

// csv::Reader deliberately tolerates malformed quotes. Reject them before they
// can merge two exported rows or swallow a column boundary.
fn validate_quotes(raw: &[u8]) -> Result<()> {
    #[derive(Clone, Copy)]
    enum Cell {
        Start,
        Plain,
        Quoted,
        AfterQuote,
    }
    use Cell::*;
    let mut state = Start;
    for &byte in raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw) {
        state = match (state, byte) {
            (Start | AfterQuote, b'"') => Quoted,
            (Quoted, b'"') => AfterQuote,
            (Quoted, _) => Quoted,
            (_, b',' | b'\r' | b'\n') => Start,
            (AfterQuote, _) | (Plain, b'"') => bail!("malformed quote in CSV"),
            _ => Plain,
        };
    }
    ensure!(!matches!(state, Quoted), "unterminated quoted cell in CSV");
    Ok(())
}

fn table_link(origin: Option<&str>, base: &str, table: &Table) -> Result<String> {
    let Some(origin) = origin else {
        return Ok(String::new());
    };
    let host = origin
        .strip_prefix("https://")
        .context("FEISHU_BASE_ORIGIN must be a Feishu HTTPS origin")?;
    let host = host.strip_suffix('/').unwrap_or(host).to_ascii_lowercase();
    ensure!(
        host.len() <= 253
            && host
                .strip_suffix(".feishu.cn")
                .is_some_and(|sub| sub.split('.').all(|s| {
                    !s.is_empty()
                        && s.len() <= 63
                        && !s.starts_with('-')
                        && !s.ends_with('-')
                        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })),
        "invalid FEISHU_BASE_ORIGIN"
    );
    Ok(format!(
        "https://{host}/base/{base}?table={}",
        table.table_id
    ))
}

fn parse(reader: impl Read, t: &Table, link: &str) -> Result<(Snapshot, Keys)> {
    let mut raw = Vec::new();
    reader.take(MAX_BYTES as u64 + 1).read_to_end(&mut raw)?;
    ensure!(raw.len() <= MAX_BYTES, "CSV exceeds 64 MiB");
    std::str::from_utf8(&raw).context("CSV must be UTF-8")?;
    validate_quotes(&raw)?;
    let mut csv = csv::Reader::from_reader(raw.as_slice());
    let headers = csv.headers().context("invalid CSV header")?.clone();
    let unique: BTreeSet<_> = headers.iter().collect();
    ensure!(unique.len() == headers.len(), "duplicate CSV headers");
    let columns = t
        .fields
        .names()
        .into_iter()
        .map(|name| {
            let i = headers
                .iter()
                .position(|h| h == name)
                .with_context(|| format!("missing mapped header {name}"))?;
            Ok((name, i))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut items = Snapshot::new();
    let mut disabled = Keys::new();
    for (index, cells) in csv.records().enumerate() {
        let number = index + 2;
        let cells = cells.with_context(|| format!("invalid CSV record {number}"))?;
        let mut row = Record {
            record_id: String::new(),
            fields: columns
                .iter()
                .map(|(name, i)| (name.to_string(), Value::String(cells[*i].to_owned())))
                .collect(),
        };
        let Some(key) =
            mapping::identity(&row, &t.fields).with_context(|| format!("CSV record {number}"))?
        else {
            continue;
        };
        ensure!(
            !items.contains_key(&key) && !disabled.contains(&key),
            "duplicate (shop, stable ID) in CSV record {number}"
        );
        let status = row.fields[&t.fields.enabled].as_str().unwrap();
        // Honor literal labels first. Native checkbox exports can uppercase true/false.
        let is_enabled = if t.enabled_values.iter().any(|s| s == status) {
            true
        } else if t.disabled_values.iter().any(|s| s == status) {
            false
        } else if status == "TRUE" && t.enabled_values.iter().any(|s| s == "true") {
            true
        } else if status == "FALSE" && t.disabled_values.iter().any(|s| s == "false") {
            false
        } else {
            bail!("unrecognized publication status in CSV record {number}");
        };
        if !is_enabled {
            disabled.insert(key);
            continue;
        }
        row.fields
            .insert(t.fields.enabled.clone(), Value::Bool(true));
        let product = row.fields[&t.fields.product_ids].as_str().unwrap();
        ensure!(
            product.trim_start().starts_with('[')
                || !product.contains([',', '，', ';', '；', '、']),
            "ambiguous product IDs in CSV record {number}; use a JSON array of strings"
        );
        let item = mapping::mapped_item(t, &row, &key, || link.to_owned())
            .with_context(|| format!("invalid fields in CSV record {number}"))?
            .with_context(|| {
                format!(
                    "incomplete enabled CSV record {number}: check scope, product IDs and content"
                )
            })?;
        if let Some(src) = item["source"].as_str().filter(|s| !s.is_empty()) {
            let url = reqwest::Url::parse(src)
                .with_context(|| format!("invalid source link in CSV record {number}"))?;
            ensure!(
                matches!(url.scheme(), "https" | "http")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "invalid source link in CSV record {number}"
            );
        }
        items.insert(key, item);
    }
    Ok((items, disabled))
}

pub fn run(t: &Table, file: &str, state: &mut State, api: &Admin) -> Result<()> {
    let path = Path::new(file);
    ensure!(
        path.extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("csv")),
        "expected a .csv export"
    );
    let base = base_token()?;
    let link = table_link(env::var("FEISHU_BASE_ORIGIN").ok().as_deref(), &base, t)?;
    let (items, disabled) = parse(File::open(path).context("open Feishu CSV")?, t, &link)?;
    let source = source_key(&base, t);
    state.bind(&api.url)?;
    state.publish(&source, api)?;
    let changes = state.stage_csv(&source, &items, &disabled)?;
    state.publish(&source, api)?;
    println!(
        "{}: {} enabled, {} explicitly disabled, {changes} changes",
        t.name,
        items.len(),
        disabled.len()
    );
    Ok(())
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
