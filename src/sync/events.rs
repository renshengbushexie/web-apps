use crate::mapping::Table;
use aes::Aes256;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    env,
    net::{SocketAddr, TcpListener},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const MAX_EVENTS: usize = 4096;
const MAX_PAYLOAD: usize = 256 * 1024;
const TYPE: &str = "drive.file.bitable_record_changed_v1";

#[derive(Default)]
struct Recent {
    ids: HashSet<String>,
    order: VecDeque<String>,
}
impl Recent {
    fn insert(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_owned()) {
            return false;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > MAX_EVENTS
            && let Some(old) = self.order.pop_front()
        {
            self.ids.remove(&old);
        }
        true
    }
}
struct Config {
    token: String,
    encrypt_key: Option<String>,
    app: String,
    base: String,
    tables: HashSet<String>,
    recent: Mutex<Recent>,
    wake: SyncSender<()>,
}
type EventResult = std::result::Result<Json<Value>, StatusCode>;
const DENIED: StatusCode = StatusCode::UNAUTHORIZED;
fn same(value: &Value, secret: &str) -> bool {
    value
        .as_str()
        .is_some_and(|v| bool::from(v.as_bytes().ct_eq(secret.as_bytes())))
}
fn valid_signature(headers: &HeaderMap, raw: &[u8], key: &str, now: u64) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let (Some(ts), Some(nonce), Some(provided)) = (
        header("x-lark-request-timestamp"),
        header("x-lark-request-nonce"),
        header("x-lark-signature"),
    ) else {
        return false;
    };
    if !ts.parse::<u64>().is_ok_and(|n| now.abs_diff(n) <= 300) || nonce.is_empty() {
        return false;
    }
    let hash = Sha256::new()
        .chain_update(ts)
        .chain_update(nonce)
        .chain_update(key)
        .chain_update(raw)
        .finalize();
    bool::from(format!("{hash:x}").as_bytes().ct_eq(provided.as_bytes()))
}
fn decrypted(body: &Value, key: &str) -> std::result::Result<Value, StatusCode> {
    let bytes = STANDARD
        .decode(body["encrypt"].as_str().ok_or(DENIED)?)
        .map_err(|_| DENIED)?;
    let (iv, data) = bytes.split_at_checked(16).ok_or(DENIED)?;
    let cipher = cbc::Decryptor::<Aes256>::new_from_slices(&Sha256::digest(key.as_bytes()), iv)
        .map_err(|_| DENIED)?;
    let plain = cipher
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| DENIED)?;
    serde_json::from_slice(&plain).map_err(|_| DENIED)
}
fn process(cfg: &Config, headers: &HeaderMap, raw: &[u8], now: u64) -> EventResult {
    let outer: Value = serde_json::from_slice(raw).map_err(|_| StatusCode::BAD_REQUEST)?;
    let message = if let Some(key) = cfg.encrypt_key.as_deref() {
        // The URL verification handshake can omit the delivery signature. It never queues a sync.
        let signed_ok = valid_signature(headers, raw, key, now);
        if headers.contains_key("x-lark-signature") && !signed_ok {
            return Err(DENIED);
        }
        let message = decrypted(&outer, key)?;
        if !signed_ok && message["type"] != "url_verification" {
            return Err(DENIED);
        }
        message
    } else {
        outer
    };
    if message["type"] == "url_verification" {
        if !same(&message["token"], &cfg.token) {
            return Err(DENIED);
        }
        let challenge = message["challenge"]
            .as_str()
            .filter(|c| !c.is_empty() && c.len() <= 1024)
            .ok_or(StatusCode::BAD_REQUEST)?;
        return Ok(Json(json!({"challenge":challenge})));
    }
    let hdr = &message["header"];
    if message["schema"] != "2.0"
        || !same(&hdr["token"], &cfg.token)
        || hdr["app_id"].as_str() != Some(&cfg.app)
    {
        return Err(DENIED);
    }
    let event = &message["event"];
    if hdr["event_type"] == TYPE
        && event["file_token"].as_str() == Some(&cfg.base)
        && event["table_id"]
            .as_str()
            .is_some_and(|id| cfg.tables.contains(id))
    {
        let id = hdr["event_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or(StatusCode::BAD_REQUEST)?;
        if cfg
            .recent
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .insert(id)
        {
            match cfg.wake.try_send(()) {
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => (),
                Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
            }
        }
    }
    Ok(Json(json!({})))
}
async fn handle(State(cfg): State<Arc<Config>>, headers: HeaderMap, body: Bytes) -> EventResult {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .as_secs();
    process(&cfg, &headers, &body, now)
}
fn router(cfg: Config) -> Router {
    Router::new()
        .route("/feishu/events", post(handle))
        .layer(DefaultBodyLimit::max(MAX_PAYLOAD))
        .with_state(Arc::new(cfg))
}
pub fn start(tables: &[Table], base: &str) -> Result<Option<Receiver<()>>> {
    let Some(bind) = env::var_os("FEISHU_EVENT_BIND") else {
        return Ok(None);
    };
    let bind: SocketAddr = bind
        .to_str()
        .context("FEISHU_EVENT_BIND must be IP:port")?
        .parse()?;
    let (wake, receiver) = sync_channel(1);
    let encrypt_key = env::var_os("FEISHU_ENCRYPT_KEY")
        .map(|s| {
            s.into_string()
                .map_err(|_| anyhow::anyhow!("FEISHU_ENCRYPT_KEY must be UTF-8"))
        })
        .transpose()?;
    if let Some(key) = encrypt_key.as_ref() {
        anyhow::ensure!(!key.trim().is_empty(), "empty FEISHU_ENCRYPT_KEY");
    }
    let cfg = Config {
        app: crate::required("FEISHU_APP_ID")?,
        base: base.to_owned(),
        tables: tables.iter().map(|t| t.table_id.clone()).collect(),
        token: crate::required("FEISHU_VERIFICATION_TOKEN")?,
        encrypt_key,
        recent: Mutex::default(),
        wake,
    };
    let socket = TcpListener::bind(bind).context("bind Feishu event receiver")?;
    socket.set_nonblocking(true)?;
    let addr = socket.local_addr()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("feishu-events".into())
        .spawn(move || {
            let result = rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(socket)?;
                axum::serve(listener, router(cfg)).await
            });
            if let Err(err) = result {
                eprintln!("Feishu event receiver stopped: {err}");
            }
        })?;
    eprintln!("Feishu event receiver listening at {addr}/feishu/events");
    Ok(Some(receiver))
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
pub fn wait(events: &Option<Receiver<()>>, interval: Duration) {
    if let Some(rx) = events {
        match rx.recv_timeout(interval) {
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(250));
                let _ = rx.try_recv();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(interval),
        }
    } else {
        std::thread::sleep(interval);
    }
}
