use super::*;
use axum::{
    Form, Json, Router,
    body::Body,
    http::Request,
    routing::{get, post},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn config() -> Auth {
    Auth::configure(
        "fixture-app".into(),
        "fixture-secret".into(),
        "https://kb.example.test".into(),
    )
    .unwrap()
}
fn routes(auth: Arc<Auth>) -> Router {
    Router::new()
        .route("/auth/feishu/login", get(login))
        .route("/auth/feishu/callback", get(callback))
        .route("/auth/logout", post(logout))
        .with_state(auth)
}

async fn start(app: &Router) -> (String, String, String) {
    let req = Request::builder()
        .uri("/auth/feishu/login")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    let url = Url::parse(resp.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("accounts.feishu.cn"));
    let args: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(args["scope"], SCOPE);
    assert_eq!(args["code_challenge_method"], "S256");
    assert_eq!(
        args["redirect_uri"],
        "https://kb.example.test/auth/feishu/callback"
    );
    let cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("Path=/; Secure; HttpOnly; SameSite=Lax"));
    (
        cookie.split(';').next().unwrap().to_owned(),
        args["state"].clone(),
        args["code_challenge"].clone(),
    )
}

async fn finish(app: &Router, binding: &str, state: &str, code: &str) -> Response {
    let path = format!("/auth/feishu/callback?state={state}&code={code}");
    let req = Request::builder()
        .uri(path)
        .header(header::COOKIE, binding)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

fn session_header(res: &Response) -> HeaderMap {
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/");
    let sid = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .unwrap()
                .strip_prefix(&format!("{SESSION}="))
                .map(|s| s.split(';').next().unwrap())
        })
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, format!("{SESSION}={sid}").parse().unwrap());
    headers
}

#[tokio::test]
async fn distinct_browsers_exchange_their_own_codes_and_never_share_tokens() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/exchange", listener.local_addr().unwrap());
    let observed = Arc::new(Mutex::new(HashMap::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let expected = observed.clone();
    let requests = count.clone();
    let provider = Router::new().route(
        "/exchange",
        post(move |Form(data): Form<HashMap<String, String>>| {
            let expected = expected.clone();
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                assert_eq!(data["grant_type"], "authorization_code");
                assert_eq!(data["client_id"], "fixture-app");
                assert_eq!(
                    data["redirect_uri"],
                    "https://kb.example.test/auth/feishu/callback"
                );
                let challenge =
                    URL_SAFE_NO_PAD.encode(Sha256::digest(data["code_verifier"].as_bytes()));
                assert_eq!(expected.lock().unwrap()[&data["code"]], challenge);
                Json(
                    serde_json::json!({"code":0,"access_token":format!("private-{}",data["code"]),
                "expires_in":3000,"token_type":"Bearer"}),
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let mut settings = config();
    let conf = settings.config.as_mut().unwrap();
    conf.token_url = endpoint;
    conf.client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let auth = Arc::new(settings);
    let app = routes(auth.clone());
    let (browser_a, state_a, challenge_a) = start(&app).await;
    let (browser_b, state_b, challenge_b) = start(&app).await;
    observed
        .lock()
        .unwrap()
        .insert("a".to_string(), challenge_a);
    observed
        .lock()
        .unwrap()
        .insert("b".to_string(), challenge_b);
    assert_eq!(
        finish(&app, &browser_a, &state_b, "b").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let a_headers = session_header(&finish(&app, &browser_a, &state_a, "a").await);
    let b_headers = session_header(&finish(&app, &browser_b, &state_b, "b").await);
    assert_eq!(auth.token(&a_headers).as_deref(), Some("private-a"));
    assert_eq!(auth.token(&b_headers).as_deref(), Some("private-b"));
    assert_eq!(
        finish(&app, &browser_a, &state_a, "a").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let mut attacker = a_headers.clone();
    attacker.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://other.example"),
    );
    assert_eq!(
        logout(State(auth.clone()), attacker).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(auth.token(&a_headers).as_deref(), Some("private-a"));
    let mut same_origin = a_headers.clone();
    same_origin.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://kb.example.test"),
    );
    assert_eq!(
        logout(State(auth.clone()), same_origin).await.status(),
        StatusCode::SEE_OTHER
    );
    assert!(auth.token(&a_headers).is_none());
    assert_eq!(auth.token(&b_headers).as_deref(), Some("private-b"));
    server.abort();
}

#[tokio::test]
async fn missing_app_configuration_is_discoverable_and_preserves_service() {
    let auth = Auth::configure("".into(), "".into(), "https://kb.example.test".into()).unwrap();
    assert!(!auth.enabled());
    assert!(auth.token(&HeaderMap::new()).is_none());
    assert_eq!(
        login(State(Arc::new(auth)), HeaderMap::new())
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(Auth::configure("app".into(), "".into(), "https://kb.example.test".into()).is_err());
    for url in [
        "http://kb.test",
        "https://name:password@kb.test",
        "https://kb.test/x",
        "https://kb.test/?a=b",
    ] {
        assert!(Auth::configure("id".into(), "secret".into(), url.into()).is_err());
    }
}

#[tokio::test]
async fn expired_state_and_sessions_are_ineligible_at_request_time() {
    let auth = Arc::new(config());
    let id = random_id().unwrap();
    let state = random_id().unwrap();
    let old = Instant::now() - Duration::from_secs(1);
    {
        let mut records = auth.lock();
        records.sessions.insert(
            id.clone(),
            Session {
                bearer: "expired".into(),
                expires: old,
            },
        );
        records.pending.insert(
            id.clone(),
            Pending {
                state: state.clone(),
                verifier: Some("unused".into()),
                expires: old,
            },
        );
        records.next_prune = Some(Instant::now() + Duration::from_secs(8));
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, format!("{SESSION}={id}").parse().unwrap());
    assert!(auth.token(&headers).is_none());
    assert_eq!(
        finish(&routes(auth), &format!("{FLOW}={id}"), &state, "untrusted")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn over_capacity_new_flow_does_not_evict_existing_ones() {
    let auth = Arc::new(config());
    let deadline = Instant::now() + Duration::from_secs(90);
    {
        let mut records = auth.lock();
        for id in 0..MAX_PENDING {
            records.pending.insert(
                id.to_string(),
                Pending {
                    state: "pending".into(),
                    verifier: Some("v".into()),
                    expires: deadline,
                },
            );
        }
    }
    let resp = login(State(auth.clone()), HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(auth.lock().pending.contains_key("0"));
}
