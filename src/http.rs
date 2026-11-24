mod ai;
pub use ai::AiProvider;

use crate::engine::{Change, Engine, Item};
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

const ADMIN_BYTES: usize = 32 * 1024 * 1024;
type Fingerprint = [u8; 32];

pub struct Credentials {
    admin: Fingerprint,
    shops: HashMap<Fingerprint, Arc<str>>,
}

impl Credentials {
    pub fn parse(admin: &str, shop_json: &str, hash_json: &str) -> Result<Self> {
        ensure!(valid_token(admin), "invalid KB_ADMIN_TOKEN");
        let admin: Fingerprint = Sha256::digest(admin.as_bytes()).into();
        let mut shops = HashMap::new();
        let mut names = HashSet::new();
        for (mapping, hashed) in [(shop_json, false), (hash_json, true)] {
            let entries: HashMap<String, String> = serde_json::from_str(mapping)
                .map_err(|_| anyhow::anyhow!("shop credentials must map shop IDs to strings"))?;
            for (shop, value) in entries {
                ensure!(
                    valid_id(&shop) && names.insert(shop.clone()),
                    "invalid or repeated shop ID"
                );
                let digest = if hashed {
                    ensure!(
                        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
                        "expected a hex SHA-256 digest"
                    );
                    let mut bytes = [0; 32];
                    for (i, byte) in bytes.iter_mut().enumerate() {
                        *byte = u8::from_str_radix(&value[2 * i..2 * i + 2], 16)?;
                    }
                    bytes
                } else {
                    ensure!(valid_token(&value), "invalid shop token");
                    Sha256::digest(value.as_bytes()).into()
                };
                ensure!(
                    digest != admin && shops.insert(digest, Arc::from(shop)).is_none(),
                    "admin and shop tokens must all be different"
                );
            }
        }
        Ok(Self { admin, shops })
    }
}

fn valid_token(token: &str) -> bool {
    (1..=1024).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Clone)]
struct App {
    engine: Arc<Engine>,
    credentials: Arc<Credentials>,
    queries: Arc<Semaphore>,
    writer: Arc<Semaphore>,
    ai: Option<Arc<AiProvider>>,
}

pub fn router(engine: Arc<Engine>, credentials: Credentials, ai: Option<AiProvider>) -> Router {
    let app = App {
        engine,
        credentials: Arc::new(credentials),
        queries: Arc::new(Semaphore::new(4)),
        writer: Arc::new(Semaphore::new(1)),
        ai: ai.map(Arc::new),
    };
    let read = Router::new()
        .route("/v1/sku", get(lookup))
        .route("/v1/search", get(search))
        .route("/v1/compare", post(ai::compare))
        .layer(DefaultBodyLimit::max(8192))
        .route_layer(middleware::from_fn_with_state(app.clone(), shop_auth));
    let admin = Router::new()
        .route("/v1/admin/changes", post(apply))
        .layer(DefaultBodyLimit::max(ADMIN_BYTES))
        .route_layer(middleware::from_fn_with_state(app.clone(), admin_auth));
    Router::new()
        .route("/healthz", get(health))
        .merge(read)
        .merge(admin)
        .layer(middleware::from_fn(|req: Request, next: Next| async move {
            let mut res = next.run(req).await;
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            res
        }))
        .with_state(app)
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;
const UNAUTHORIZED: ApiError = ApiError(StatusCode::UNAUTHORIZED, "unauthorized");
const INVALID: ApiError = ApiError(StatusCode::BAD_REQUEST, "invalid_request");
const BUSY: ApiError = ApiError(StatusCode::SERVICE_UNAVAILABLE, "service_busy");
const INTERNAL: ApiError = ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");

fn bearer(headers: &HeaderMap) -> ApiResult<Fingerprint> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let auth = values
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or(UNAUTHORIZED)?;
    let (scheme, token) = auth.split_once(' ').ok_or(UNAUTHORIZED)?;
    if values.next().is_some() || !scheme.eq_ignore_ascii_case("Bearer") || !valid_token(token) {
        return Err(UNAUTHORIZED);
    }
    Ok(Sha256::digest(token.as_bytes()).into())
}

async fn shop_auth(State(app): State<App>, mut req: Request, next: Next) -> ApiResult<Response> {
    let token = bearer(req.headers())?;
    let shop = app.credentials.shops.get(&token).ok_or(UNAUTHORIZED)?;
    req.extensions_mut().insert(shop.clone());
    Ok(next.run(req).await)
}

async fn admin_auth(State(app): State<App>, mut req: Request, next: Next) -> ApiResult<Response> {
    if bearer(req.headers())? != app.credentials.admin {
        return Err(UNAUTHORIZED);
    }
    let permit = app.writer.try_acquire_owned().map_err(|_| BUSY)?;
    // Held through request parsing and publication, even when the HTTP client disconnects mid-write.
    req.extensions_mut().insert(Arc::new(permit));
    Ok(next.run(req).await)
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && s.trim() == s && !s.chars().any(char::is_control)
}

async fn health(State(app): State<App>) -> Json<serde_json::Value> {
    Json(json!({"status":"ok", "version":app.engine.version()}))
}

impl App {
    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Engine) -> T + Send + 'static,
    ) -> ApiResult<T> {
        let permit = tokio::time::timeout(
            Duration::from_millis(250),
            self.queries.clone().acquire_owned(),
        )
        .await
        .map_err(|_| BUSY)?
        .map_err(|_| BUSY)?;
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f(&engine)
        })
        .await
        .map_err(|_| INTERNAL)
    }
}

#[derive(serde::Serialize)]
struct MatchItem {
    id: String,
    sku: String,
    model: String,
    question: String,
    answer: String,
    source: String,
}
impl From<Item> for MatchItem {
    fn from(item: Item) -> Self {
        Self {
            id: item.id,
            sku: item.sku,
            model: item.model,
            question: item.question,
            answer: item.answer,
            source: item.source,
        }
    }
}
#[derive(serde::Serialize)]
struct Matches {
    version: u64,
    items: Vec<MatchItem>,
}
impl Matches {
    fn new(version: u64, items: Vec<Item>) -> Json<Self> {
        Json(Self {
            version,
            items: items.into_iter().map(MatchItem::from).collect(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkuQuery {
    product_id: String,
    #[serde(default)]
    sku: String,
    #[serde(default)]
    model: String,
}

async fn lookup(
    State(app): State<App>,
    Extension(shop): Extension<Arc<str>>,
    Query(q): Query<SkuQuery>,
) -> ApiResult<Json<Matches>> {
    if !valid_id(&q.product_id)
        || (!q.sku.is_empty() && !valid_id(&q.sku))
        || (!q.model.is_empty() && !valid_id(&q.model))
    {
        return Err(INVALID);
    }
    let (version, items) = app
        .read(move |e| e.lookup(&shop, &q.product_id, &q.sku, &q.model))
        .await?;
    Ok(Matches::new(version, items))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    product_id: String,
    q: String,
    limit: Option<usize>,
}

async fn search(
    State(app): State<App>,
    Extension(shop): Extension<Arc<str>>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Matches>> {
    let limit = q.limit.unwrap_or(5);
    if !valid_id(&q.product_id) || !valid_id(&q.q) || !(1..=20).contains(&limit) {
        return Err(INVALID);
    }
    let (version, items) = app
        .read(move |e| e.search(&shop, &q.product_id, &q.q, limit))
        .await?;
    Ok(Matches::new(version, items))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    changes: Vec<Change>,
}

async fn apply(
    State(app): State<App>,
    Extension(permit): Extension<Arc<tokio::sync::OwnedSemaphorePermit>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected_application_json",
        ));
    }
    let version = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let batch: Batch = serde_json::from_slice(&body).map_err(|_| INVALID)?;
        if batch.changes.len() > 50_000 {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large"));
        }
        app.engine.apply(batch.changes).map_err(|err| {
            eprintln!("Knowledge publication failed: {err}");
            ApiError(StatusCode::UNPROCESSABLE_ENTITY, "publication_failed")
        })
    })
    .await
    .map_err(|_| INTERNAL)??;
    Ok(Json(json!({"version":version})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Method,
    };
    use serde_json::Value;
    use std::path::Path;
    use tower::ServiceExt;

    const ADMIN: &str = "test-admin-abcdefghijklmnopqrstuvwxyz";
    const SHOP_A: &str = "test-shop-a-abcdefghijklmnopqrstuvwxyz";
    const SHOP_B: &str = "test-shop-b-abcdefghijklmnopqrstuvwxyz";

    fn app() -> Router {
        let credentials =
            Credentials::parse(ADMIN, &json!({"a":SHOP_A,"b":SHOP_B}).to_string(), "{}").unwrap();
        router(
            Arc::new(Engine::open(Path::new(":memory:")).unwrap()),
            credentials,
            None,
        )
    }

    async fn call(
        app: &Router,
        method: Method,
        path: &str,
        token: &str,
        payload: Value,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        let status = res.status();
        let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[test]
    fn handles_custom_keys_or_digests_but_refuses_cross_shop_credentials() {
        assert!(Credentials::parse(ADMIN, &json!({"a":ADMIN}).to_string(), "{}").is_err());
        assert!(
            Credentials::parse(ADMIN, &json!({"a":SHOP_A,"b":SHOP_A}).to_string(), "{}").is_err()
        );
        assert!(Credentials::parse("", &json!({"a":SHOP_A}).to_string(), "{}").is_err());
        let hash = format!("{:x}", Sha256::digest("my-own-key"));
        let credentials = Credentials::parse(ADMIN, "{}", &json!({"a":hash}).to_string()).unwrap();
        let digest: Fingerprint = Sha256::digest("my-own-key").into();
        assert_eq!(credentials.shops[&digest].as_ref(), "a");
        assert!(
            Credentials::parse(
                ADMIN,
                &json!({"b":"my-own-key"}).to_string(),
                &json!({"a":hash}).to_string()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn unprovisioned_instance_has_no_read_access() {
        assert!(Credentials::parse("", "{}", "{}").is_err());
        let credentials = Credentials::parse(ADMIN, "{}", "{}").unwrap();
        let app = router(
            Arc::new(Engine::open(Path::new(":memory:")).unwrap()),
            credentials,
            None,
        );
        for key in ["", "future-customer-key", ADMIN] {
            let (status, _) =
                call(&app, Method::GET, "/v1/sku?product_id=p", key, Value::Null).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let health = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enforces_tenant_and_endpoint_credentials() {
        let app = app();
        let no_token = Request::builder()
            .method("POST")
            .uri("/v1/admin/changes")
            .body(Body::from("invalid JSON"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(no_token).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let payload = json!({"changes":[{"shop":"a","id":"42","item":{
            "shop":"a","id":"42","kind":"sku","scope":"product","product_ids":["00001","another-product"],
            "sku":"sku-a","model":"Phone 17 Pro","question":"","answer":"A only",
            "source":"https://example.com/source","enabled":true
        }}]});
        assert_eq!(
            call(
                &app,
                Method::POST,
                "/v1/admin/changes",
                SHOP_A,
                payload.clone()
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, Method::POST, "/v1/admin/changes", ADMIN, payload)
                .await
                .0,
            StatusCode::OK
        );
        let url = "/v1/sku?product_id=00001&sku=sku-a";
        let (status, own) = call(&app, Method::GET, url, SHOP_A, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(own["items"][0]["answer"], "A only");
        assert!(own["items"][0].get("product_ids").is_none());
        let (_, other) = call(&app, Method::GET, url, SHOP_B, Value::Null).await;
        assert_eq!(other["items"], json!([]));
        assert_eq!(
            call(&app, Method::GET, url, ADMIN, Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                &app,
                Method::GET,
                "/v1/sku?product_id=00001&shop=a",
                SHOP_B,
                Value::Null
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn refuses_excess_results_and_disables_unconfigured_ai() {
        let app = app();
        assert_eq!(
            call(
                &app,
                Method::GET,
                "/v1/search?product_id=p&q=word&limit=21",
                SHOP_A,
                Value::Null
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let body = json!({"left_product_id":"one","right_product_id":"two","question":"compare"});
        assert_eq!(
            call(&app, Method::POST, "/v1/compare", SHOP_A, body)
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
