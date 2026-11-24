//! Raw, request-scoped Feishu reads. Field operators are returned by `/me/fields`.
//! Text supports equality/substrings; number, single select and auto-number equality;
//! multi-select supports equality/membership. Numbers are submitted as the caller's
//! original decimal text. Rows represent the human user's visible Base data.
use super::auth::Auth;
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, OnceLock},
    time::Duration,
};

const ORIGIN: &str = "https://open.feishu.cn/open-apis";
const ALLOWED: [&str; 3] = ["SKU明细", "商品QA", "晓多商品知识"];
const BODY_LIMIT: usize = 4 * 1024 * 1024;
type Session = Arc<dyn Fn(&HeaderMap) -> Option<Arc<str>> + Send + Sync>;

#[derive(Clone)]
struct Knowledge {
    session: Session,
    source: Option<Source>,
}
#[derive(Clone)]
struct Source {
    client: Client,
    root: String,
    base: String,
    web_origin: Option<String>,
}
#[derive(Clone, Copy)]
struct Failure(StatusCode, &'static str);
type Result<T> = std::result::Result<T, Failure>;
const LOGIN: Failure = Failure(StatusCode::UNAUTHORIZED, "login_required");
const EXPIRED: Failure = Failure(StatusCode::UNAUTHORIZED, "feishu_session_expired");
const FORBIDDEN: Failure = Failure(StatusCode::FORBIDDEN, "feishu_access_denied");
const NO_TABLE: Failure = Failure(StatusCode::NOT_FOUND, "table_unavailable");
const INVALID: Failure = Failure(StatusCode::BAD_REQUEST, "invalid_query");
const UPSTREAM: Failure = Failure(StatusCode::BAD_GATEWAY, "feishu_read_failed");
const DEADLINE: Failure = Failure(StatusCode::GATEWAY_TIMEOUT, "feishu_timeout");
const LIMITED: Failure = Failure(StatusCode::TOO_MANY_REQUESTS, "feishu_rate_limited");
const NOT_CONFIGURED: Failure =
    Failure(StatusCode::SERVICE_UNAVAILABLE, "feishu_base_unconfigured");
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
#[derive(Deserialize, Serialize)]
struct Table {
    table_id: String,
    name: String,
}
#[derive(Deserialize)]
struct Field {
    field_name: String,
    field_id: String,
    #[serde(rename = "type")]
    kind: u32,
}
#[derive(Deserialize)]
struct Record {
    record_id: String,
    fields: BTreeMap<String, Value>,
    record_url: Option<String>,
}
#[derive(Deserialize)]
struct Page<T> {
    items: Option<Vec<T>>,
    has_more: bool,
    page_token: Option<String>,
}

fn deployed() -> Option<&'static Source> {
    static SOURCE: OnceLock<Option<Source>> = OnceLock::new();
    SOURCE
        .get_or_init(|| {
            let base = std::env::var("FEISHU_BASE_TOKEN")
                .ok()
                .filter(|v| safe_id(v))?;
            let client = Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?;
            Some(Source {
                root: format!("{ORIGIN}/bitable/v1/apps/{base}/tables"),
                base,
                web_origin: std::env::var("FEISHU_BASE_ORIGIN")
                    .ok()
                    .and_then(|s| tenant_origin(&s)),
                client,
            })
        })
        .as_ref()
}
pub(super) fn tenant_origin(raw: &str) -> Option<String> {
    let authority = raw.strip_prefix("https://")?;
    let host = authority
        .strip_suffix('/')
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let sub = host.strip_suffix(".feishu.cn")?;
    if host.len() > 253
        || !sub.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return None;
    }
    Some(format!("https://{host}"))
}
fn record_link(
    origin: Option<&str>,
    base: &str,
    table: &str,
    record: &str,
    native: Option<&str>,
) -> Option<String> {
    let origin = origin?;
    let path = format!("/base/{base}");
    let mut url = reqwest::Url::parse(&format!("{origin}{path}")).ok()?;
    url.query_pairs_mut().append_pair("table", table);
    // Keep an optional native view identifier; all navigation targets remain on the deployed tenant.
    if let Some(src) = native.and_then(|s| reqwest::Url::parse(s).ok())
        && src.origin().ascii_serialization() == origin
        && src.path() == path
        && src.username().is_empty()
        && src.password().is_none()
        && src.query_pairs().any(|(k, v)| k == "table" && v == table)
        && src.query_pairs().any(|(k, v)| k == "record" && v == record)
        && let Some((_, view)) = src
            .query_pairs()
            .find(|(k, v)| k == "view" && v.starts_with("vew") && safe_id(v))
    {
        url.query_pairs_mut().append_pair("view", &view);
    }
    url.query_pairs_mut().append_pair("record", record);
    Some(url.into())
}
pub fn router(auth: Arc<Auth>) -> anyhow::Result<Router> {
    Ok(routes(Knowledge {
        source: deployed().cloned(),
        session: Arc::new(move |headers| auth.token(headers)),
    }))
}
fn routes(state: Knowledge) -> Router {
    Router::new()
        .route("/me/tables", get(tables))
        .route("/me/fields", get(fields))
        .route("/me/records", get(records))
        .layer(middleware::from_fn(private_response))
        .with_state(state)
}
async fn private_response(req: Request<Body>, next: Next) -> Response {
    let mut res = tokio::time::timeout(Duration::from_secs(25), next.run(req))
        .await
        .unwrap_or_else(|_| DEADLINE.into_response());
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    res.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static("Cookie, Authorization"),
    );
    res.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}
pub(super) fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn safe_value(s: &str, limit: usize) -> bool {
    !s.trim().is_empty() && s.len() <= limit && !s.chars().any(char::is_control)
}
fn allowed(name: &str) -> bool {
    let normalized: String = name.chars().filter(|c| !c.is_whitespace()).collect();
    ALLOWED.contains(&normalized.as_str())
}
fn operators(kind: u32) -> &'static [&'static str] {
    match kind {
        1 => &["is", "contains"],
        2 | 3 | 1005 => &["is"],
        4 => &["is", "contains"],
        _ => &[],
    }
}
fn readable(kind: u32) -> bool {
    matches!(
        kind,
        1 | 2 | 3 | 4 | 5 | 7 | 15 | 18 | 20 | 21 | 1001 | 1002 | 1005
    )
}
async fn fetch<T: DeserializeOwned>(req: RequestBuilder, token: &str) -> Result<T> {
    let res = req
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| if e.is_timeout() { DEADLINE } else { UPSTREAM })?;
    match res.status().as_u16() {
        200..=299 => (),
        401 => return Err(EXPIRED),
        403 => return Err(FORBIDDEN),
        404 => return Err(NO_TABLE),
        429 => return Err(LIMITED),
        _ => return Err(UPSTREAM),
    }
    let mut data = Vec::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| UPSTREAM)?;
        if data.len() + chunk.len() > BODY_LIMIT {
            return Err(UPSTREAM);
        }
        data.extend_from_slice(&chunk);
    }
    let mut body: Value = serde_json::from_slice(&data).map_err(|_| UPSTREAM)?;
    match body["code"].as_i64().ok_or(UPSTREAM)? {
        0 => serde_json::from_value(body["data"].take()).map_err(|_| UPSTREAM),
        99991663 | 99991668 | 99991677 => Err(EXPIRED),
        91403 | 1254301 | 1254302 | 1254303 | 1254304 | 99991672 | 99991679 => Err(FORBIDDEN),
        1254040 | 1254041 => Err(NO_TABLE),
        99991400 | 1254290 => Err(LIMITED),
        1254018 | 1254019 | 1254045 => Err(INVALID),
        _ => Err(UPSTREAM),
    }
}
async fn metadata<T: DeserializeOwned>(k: &Source, url: &str, token: &str) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let mut cursor = None::<String>;
    let mut seen = HashSet::new();
    for _ in 0..30 {
        let mut req = k.client.get(url).query(&[("page_size", "100")]);
        if let Some(c) = cursor.as_ref() {
            req = req.query(&[("page_token", c)]);
        }
        let page: Page<T> = fetch(req, token).await?;
        let items = page.items.unwrap_or_default();
        let n = items.len();
        out.extend(items);
        if !page.has_more {
            return Ok(out);
        }
        let next = page
            .page_token
            .filter(|c| safe_value(c, 2048))
            .ok_or(UPSTREAM)?;
        if n == 0 || !seen.insert(next.clone()) {
            return Err(UPSTREAM);
        }
        cursor = Some(next);
    }
    Err(UPSTREAM)
}
impl Knowledge {
    fn access(&self, headers: &HeaderMap) -> Result<(Arc<str>, &Source)> {
        let token = (self.session)(headers).ok_or(LOGIN)?;
        Ok((token, self.source.as_ref().ok_or(NOT_CONFIGURED)?))
    }
}
impl Source {
    async fn visible(&self, token: &str) -> Result<Vec<Table>> {
        let tables: Vec<Table> = metadata(self, &self.root, token).await?;
        Ok(tables
            .into_iter()
            .filter(|t| safe_id(&t.table_id) && allowed(&t.name))
            .collect())
    }
    async fn schema(&self, token: &str, id: &str, tables: &[Table]) -> Result<Vec<Field>> {
        if !safe_id(id) || !id.starts_with("tbl") {
            return Err(INVALID);
        }
        if !tables.iter().any(|t| t.table_id == id) {
            return Err(NO_TABLE);
        }
        metadata(self, &format!("{}/{id}/fields", self.root), token).await
    }
}
async fn tables(State(k): State<Knowledge>, h: HeaderMap) -> Result<Json<Value>> {
    let (token, src) = k.access(&h)?;
    Ok(Json(json!({"tables":src.visible(&token).await?})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableQuery {
    table_id: String,
}
async fn fields(
    State(k): State<Knowledge>,
    h: HeaderMap,
    q: std::result::Result<Query<TableQuery>, QueryRejection>,
) -> Result<Json<Value>> {
    let (token, src) = k.access(&h)?;
    let Query(q) = q.map_err(|_| INVALID)?;
    let tables = src.visible(&token).await?;
    let fields = src.schema(&token, &q.table_id, &tables).await?;
    Ok(Json(json!({"fields":descriptions(&fields)})))
}
fn descriptions(fields: &[Field]) -> Vec<Value> {
    fields.iter().filter(|f|readable(f.kind)).map(|f|json!({"field_id":f.field_id,"name":f.field_name,"type":f.kind,"operators":operators(f.kind)})).collect()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lookup {
    table_id: String,
    field: Option<String>,
    value: Option<String>,
    op: Option<String>,
    limit: Option<usize>,
    page_token: Option<String>,
}
fn search(q: &Lookup, fields: &[Field]) -> Result<(Value, HashSet<String>)> {
    let mut selected = Vec::new();
    let filter = match (&q.field, &q.value) {
        (None, None) if q.op.is_none() => None,
        (Some(field), Some(value)) if safe_value(field, 1024) && safe_value(value, 2048) => {
            let found = fields
                .iter()
                .find(|f| f.field_name == *field)
                .ok_or(INVALID)?;
            let op = q.op.as_deref().unwrap_or("is");
            if !operators(found.kind).contains(&op) {
                return Err(INVALID);
            }
            if found.kind == 2 && serde_json::from_str::<serde_json::Number>(value).is_err() {
                return Err(INVALID);
            }
            selected.push(field.clone());
            Some(
                json!({"conjunction":"and","conditions":[{"field_name":field,"operator":op,"value":[value]}]}),
            )
        }
        _ => return Err(INVALID),
    };
    for f in fields.iter().filter(|f| readable(f.kind)) {
        if selected.len() == 40 {
            break;
        }
        if !selected.contains(&f.field_name) {
            selected.push(f.field_name.clone());
        }
    }
    if selected.is_empty() {
        return Err(NO_TABLE);
    }
    let mut body = json!({"field_names":selected,"automatic_fields":false});
    if let Some(filter) = filter {
        body["filter"] = filter;
    }
    Ok((body, selected.into_iter().collect()))
}
fn cell(value: &Value) -> String {
    fn append(s: &str, budget: &mut usize, out: &mut String) {
        let part: String = s.chars().take(*budget).collect();
        *budget -= part.chars().count();
        out.push_str(&part);
    }
    fn walk(v: &Value, depth: usize, budget: &mut usize, out: &mut String) {
        if *budget == 0 || depth > 6 {
            return;
        }
        match v {
            Value::Null => (),
            Value::String(s) => append(s, budget, out),
            Value::Bool(_) | Value::Number(_) => append(&v.to_string(), budget, out),
            Value::Array(items) => {
                let rich = items.first().is_some_and(|v| v.get("text").is_some());
                for (i, v) in items.iter().enumerate() {
                    if *budget == 0 {
                        break;
                    }
                    if i > 0 && !rich {
                        append("、", budget, out);
                    }
                    walk(v, depth + 1, budget, out);
                }
            }
            Value::Object(obj) => {
                if let Some(v) = [
                    "text",
                    "name",
                    "value",
                    "link",
                    "record_ids",
                    "link_record_ids",
                ]
                .iter()
                .find_map(|key| obj.get(*key))
                {
                    walk(v, depth + 1, budget, out);
                }
            }
        }
    }
    let mut result = String::new();
    let mut budget = 2048;
    walk(value, 0, &mut budget, &mut result);
    result
}
async fn records(
    State(k): State<Knowledge>,
    h: HeaderMap,
    q: std::result::Result<Query<Lookup>, QueryRejection>,
) -> Result<Json<Value>> {
    let (token, src) = k.access(&h)?;
    let Query(q) = q.map_err(|_| INVALID)?;
    let tables = src.visible(&token).await?;
    let schema = src.schema(&token, &q.table_id, &tables).await?;
    Ok(Json(read_records(src, &token, &q, &schema).await?))
}
async fn read_records(k: &Source, token: &str, q: &Lookup, schema: &[Field]) -> Result<Value> {
    let limit = q.limit.unwrap_or(10);
    if !(1..=25).contains(&limit) || q.page_token.as_ref().is_some_and(|s| !safe_value(s, 2048)) {
        return Err(INVALID);
    }
    let (body, selected) = search(q, schema)?;
    let mut req = k
        .client
        .post(format!("{}/{}/records/search", k.root, q.table_id))
        .query(&[("page_size", limit)]);
    if let Some(cursor) = &q.page_token {
        req = req.query(&[("page_token", cursor)]);
    }
    let page: Page<Record> = fetch(req.json(&body), token).await?;
    let items = page.items.unwrap_or_default();
    if items.len() > limit
        || (page.has_more && items.is_empty())
        || (page.has_more
            && !page
                .page_token
                .as_ref()
                .is_some_and(|s| safe_value(s, 2048)))
    {
        return Err(UPSTREAM);
    }
    let mut rows = Vec::new();
    for r in items {
        if !safe_id(&r.record_id) {
            return Err(UPSTREAM);
        }
        let fields: BTreeMap<_, _> = r
            .fields
            .into_iter()
            .filter(|(n, _)| selected.contains(n))
            .map(|(n, v)| (n, cell(&v)))
            .collect();
        let source = record_link(
            k.web_origin.as_deref(),
            &k.base,
            &q.table_id,
            &r.record_id,
            r.record_url.as_deref(),
        );
        rows.push(json!({"record_id":r.record_id,"fields":fields,"source":source}));
    }
    Ok(
        json!({"records":rows,"has_more":page.has_more,"next_page_token":if page.has_more{page.page_token}else{None}}),
    )
}

pub(super) async fn view(
    auth: &Auth,
    headers: &HeaderMap,
    table: Option<&str>,
    field: Option<&str>,
    value: Option<&str>,
    op: Option<&str>,
    page_token: Option<&str>,
) -> std::result::Result<Value, (StatusCode, &'static str)> {
    let work = async {
        let token = auth.token(headers).ok_or(LOGIN)?;
        let src = deployed().ok_or(NOT_CONFIGURED)?;
        let tables = src.visible(&token).await?;
        let Some(table) = table else {
            return Ok(json!({"tables":tables}));
        };
        let schema = src.schema(&token, table, &tables).await?;
        let q = Lookup {
            table_id: table.into(),
            field: field.map(str::to_owned),
            value: value.map(str::to_owned),
            op: op.map(str::to_owned),
            page_token: page_token.map(str::to_owned),
            limit: Some(10),
        };
        let mut page = read_records(src, &token, &q, &schema).await?;
        page["tables"] = json!(tables);
        page["fields"] = json!(descriptions(&schema));
        Ok::<_, Failure>(page)
    };
    tokio::time::timeout(Duration::from_secs(25), work)
        .await
        .unwrap_or(Err(DEADLINE))
        .map_err(|err| (err.0, err.1))
}

#[cfg(test)]
#[path = "knowledge/tests.rs"]
mod tests;
