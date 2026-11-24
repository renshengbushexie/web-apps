//! Browser-bound Feishu authorization code flow for personal reads.
use anyhow::{Context, Result, ensure};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

const AUTHORIZE: &str = "https://accounts.feishu.cn/open-apis/authen/v1/authorize";
const TOKEN_URL: &str = "https://accounts.feishu.cn/oauth/v3/token";
const SCOPE: &str = "bitable:app:readonly";
const FLOW: &str = "__Host-kb_oauth";
const SESSION: &str = "__Host-kb_session";
const FLOW_SECONDS: u64 = 300;
const MAX_PENDING: usize = 4096;
const MAX_SESSIONS: usize = 8192;

struct Config {
    app_id: String,
    secret: String,
    origin: String,
    callback: String,
    client: Client,
    token_url: String,
}
struct Pending {
    state: String,
    verifier: Option<String>,
    expires: Instant,
}
struct Session {
    bearer: Arc<str>,
    expires: Instant,
}
#[derive(Default)]
struct Records {
    pending: HashMap<String, Pending>,
    sessions: HashMap<String, Session>,
    next_prune: Option<Instant>,
}
impl Records {
    fn prune(&mut self, now: Instant) {
        if self.next_prune.is_none_or(|t| t <= now) {
            self.pending.retain(|_, item| item.expires > now);
            self.sessions.retain(|_, item| item.expires > now);
            self.next_prune = Some(now + Duration::from_secs(10));
        }
    }
}

pub(super) struct Auth {
    config: Option<Config>,
    records: Mutex<Records>,
    exchange_slots: Semaphore,
}

impl Auth {
    pub(super) fn from_env() -> Result<Self> {
        Self::configure(
            env::var("FEISHU_WEB_APP_ID").unwrap_or_default(),
            env::var("FEISHU_WEB_APP_SECRET").unwrap_or_default(),
            env::var("KB_PUBLIC_ORIGIN").unwrap_or_default(),
        )
    }

    fn configure(app_id: String, secret: String, origin: String) -> Result<Self> {
        let config = if app_id.is_empty() && secret.is_empty() {
            None
        } else {
            ensure!(
                !app_id.trim().is_empty() && !secret.trim().is_empty(),
                "configure both FEISHU_WEB_APP_ID and FEISHU_WEB_APP_SECRET"
            );
            let url = Url::parse(&origin).context("KB_PUBLIC_ORIGIN must be an HTTPS origin")?;
            ensure!(
                url.scheme() == "https"
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "KB_PUBLIC_ORIGIN must be an HTTPS origin without a path or credentials"
            );
            let origin = url.origin().ascii_serialization();
            let callback = format!("{origin}/auth/feishu/callback");
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()?;
            Some(Config {
                app_id,
                secret,
                origin,
                callback,
                client,
                token_url: TOKEN_URL.into(),
            })
        };
        Ok(Self {
            config,
            records: Mutex::new(Records::default()),
            exchange_slots: Semaphore::new(16),
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.config.is_some()
    }

    fn lock(&self) -> MutexGuard<'_, Records> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn token(&self, headers: &HeaderMap) -> Option<Arc<str>> {
        let id = cookie(headers, SESSION)?;
        let now = Instant::now();
        let mut records = self.lock();
        records.prune(now);
        records
            .sessions
            .get(id)
            .filter(|session| session.expires > now)
            .map(|s| s.bearer.clone())
    }
}

fn random_id() -> Result<String, StatusCode> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        for pair in header.to_str().ok()?.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if key == name {
                if found.is_some()
                    || value.len() != 43
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}

fn response(status: StatusCode, text: &'static str) -> Response {
    let mut res = (status, text).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

fn set_cookie(res: &mut Response, name: &str, value: &str, ttl: u64) {
    let cookie = format!("{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={ttl}");
    res.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("ASCII cookie"),
    );
}
fn home() -> Response {
    let mut res = response(StatusCode::SEE_OTHER, "");
    res.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    res
}

pub(super) async fn login(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    let Some(cfg) = &auth.config else {
        return response(StatusCode::SERVICE_UNAVAILABLE, "飞书网页登录尚未配置。");
    };
    let (Ok(flow), Ok(state), Ok(verifier)) = (random_id(), random_id(), random_id()) else {
        return response(StatusCode::SERVICE_UNAVAILABLE, "暂时无法开始登录。");
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = Url::parse(AUTHORIZE).expect("static Feishu authorization URL");
    url.query_pairs_mut().extend_pairs([
        ("client_id", cfg.app_id.as_str()),
        ("redirect_uri", cfg.callback.as_str()),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    {
        let mut r = auth.lock();
        let now = Instant::now();
        r.prune(now);
        if let Some(old) = cookie(&headers, FLOW) {
            r.pending.remove(old);
        }
        if r.pending.len() >= MAX_PENDING {
            return response(StatusCode::TOO_MANY_REQUESTS, "登录请求繁忙，请稍后再试。");
        }
        r.pending.insert(
            flow.clone(),
            Pending {
                state,
                verifier: Some(verifier),
                expires: now + Duration::from_secs(FLOW_SECONDS),
            },
        );
    }
    let mut res = response(StatusCode::SEE_OTHER, "");
    res.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(url.as_str()).expect("encoded authorization URL"),
    );
    set_cookie(&mut res, FLOW, &flow, FLOW_SECONDS);
    res
}

#[derive(Deserialize)]
pub(super) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}
#[derive(Deserialize)]
struct Grant {
    code: i64,
    access_token: String,
    expires_in: u64,
    token_type: String,
}

async fn exchange(cfg: &Config, code: &str, verifier: &str) -> Option<Grant> {
    let body = [
        ("grant_type", "authorization_code"),
        ("client_id", &cfg.app_id),
        ("client_secret", &cfg.secret),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", &cfg.callback),
        ("scope", SCOPE),
    ];
    let mut reply = cfg
        .client
        .post(&cfg.token_url)
        .form(&body)
        .send()
        .await
        .ok()?;
    if !reply.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await.ok()? {
        if bytes.len() + chunk.len() > 32 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let grant: Grant = serde_json::from_slice(&bytes).ok()?;
    if grant.code != 0
        || !grant.token_type.eq_ignore_ascii_case("bearer")
        || grant.expires_in <= 5
        || !(1..=16384).contains(&grant.access_token.len())
        || !grant.access_token.bytes().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    Some(grant)
}

fn invalid_flow() -> Response {
    response(
        StatusCode::BAD_REQUEST,
        "本次授权与浏览器不匹配或已过期，请返回首页重新登录。",
    )
}

pub(super) async fn callback(
    State(auth): State<Arc<Auth>>,
    headers: HeaderMap,
    args: Result<Query<Callback>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Some(cfg) = &auth.config else {
        return response(StatusCode::SERVICE_UNAVAILABLE, "飞书网页登录尚未配置。");
    };
    let (Ok(Query(params)), Some(flow)) = (args, cookie(&headers, FLOW)) else {
        return invalid_flow();
    };
    let Some(supplied) = params.state.filter(|s| s.len() == 43) else {
        return invalid_flow();
    };
    let verifier = {
        let mut r = auth.lock();
        let Some(pending) = r.pending.get_mut(flow) else {
            return invalid_flow();
        };
        if pending.expires <= Instant::now()
            || !bool::from(pending.state.as_bytes().ct_eq(supplied.as_bytes()))
        {
            return invalid_flow();
        }
        let Some(verifier) = pending.verifier.take() else {
            return invalid_flow();
        };
        verifier
    };
    let mut res = if let Some(code) = params
        .code
        .filter(|s| !s.is_empty() && s.len() <= 2048 && params.error.is_none())
    {
        let Ok(_slot) = auth.exchange_slots.try_acquire() else {
            auth.lock().pending.remove(flow);
            return response(StatusCode::TOO_MANY_REQUESTS, "登录服务繁忙，请重新登录。");
        };
        let started = Instant::now();
        let grant = exchange(cfg, &code, &verifier).await;
        let mut r = auth.lock();
        let now = Instant::now();
        let still_valid = r
            .pending
            .remove(flow)
            .is_some_and(|pending| pending.expires > now);
        if !still_valid {
            return invalid_flow();
        }
        let Some(grant) = grant else {
            return response(
                StatusCode::BAD_GATEWAY,
                "飞书授权未完成，请返回首页重新登录。",
            );
        };
        // Subtract transit time and a safety margin; only the provider can extend consent.
        let expires = started + Duration::from_secs(grant.expires_in.saturating_sub(5).min(7200));
        let ttl = expires.saturating_duration_since(now).as_secs();
        if ttl == 0 {
            return invalid_flow();
        }
        let Ok(sid) = random_id() else {
            return response(StatusCode::SERVICE_UNAVAILABLE, "暂时无法建立会话。");
        };
        r.prune(now);
        if let Some(old) = cookie(&headers, SESSION) {
            r.sessions.remove(old);
        }
        if r.sessions.len() >= MAX_SESSIONS {
            return response(StatusCode::TOO_MANY_REQUESTS, "登录会话已满，请稍后再试。");
        }
        r.sessions.insert(
            sid.clone(),
            Session {
                bearer: Arc::from(grant.access_token),
                expires,
            },
        );
        let mut res = home();
        set_cookie(&mut res, SESSION, &sid, ttl);
        res
    } else {
        auth.lock().pending.remove(flow);
        response(StatusCode::FORBIDDEN, "飞书授权已取消，请返回首页。")
    };
    set_cookie(&mut res, FLOW, "", 0);
    res
}

pub(super) async fn logout(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    let Some(cfg) = &auth.config else {
        return response(StatusCode::SERVICE_UNAVAILABLE, "飞书网页登录尚未配置。");
    };
    let mut origins = headers.get_all(header::ORIGIN).iter();
    if origins.next().and_then(|v| v.to_str().ok()) != Some(&cfg.origin) || origins.next().is_some()
    {
        return response(StatusCode::FORBIDDEN, "无法验证退出请求来源。");
    }
    let mut records = auth.lock();
    if let Some(id) = cookie(&headers, SESSION) {
        records.sessions.remove(id);
    }
    if let Some(id) = cookie(&headers, FLOW) {
        records.pending.remove(id);
    }
    let mut res = home();
    set_cookie(&mut res, SESSION, "", 0);
    set_cookie(&mut res, FLOW, "", 0);
    res
}

#[cfg(test)]
mod tests;
