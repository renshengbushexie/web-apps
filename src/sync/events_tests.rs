use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

fn config(key: Option<&str>) -> (Config, Receiver<()>) {
    let (wake, rx) = sync_channel(1);
    (
        Config {
            token: "verified".into(),
            encrypt_key: key.map(str::to_owned),
            app: "demo-app".into(),
            base: "base".into(),
            tables: ["tblA".into()].into(),
            recent: Mutex::default(),
            wake,
        },
        rx,
    )
}
fn event(id: &str, table: &str) -> Value {
    json!({"schema":"2.0","header":{"app_id":"demo-app","event_type":TYPE,"event_id":id,"token":"verified"},
        "event":{"file_token":"base","table_id":table}})
}
#[test]
fn token_scope_deduplication_and_coalescing() {
    let (cfg, rx) = config(None);
    let send =
        |v: Value| process(&cfg, &HeaderMap::new(), v.to_string().as_bytes(), 0).map(|j| j.0);
    assert_eq!(
        send(json!({"type":"url_verification","challenge":"hello","token":"verified"})).unwrap()["challenge"],
        "hello"
    );
    assert_eq!(
        send(json!({"type":"url_verification","challenge":"hello","token":"wrong"})).unwrap_err(),
        DENIED
    );
    let mut wrong = event("a", "tblA");
    wrong["header"]["app_id"] = json!("other");
    assert_eq!(send(wrong).unwrap_err(), DENIED);
    let mut wrong = event("a", "tblA");
    wrong["header"]["token"] = json!("other");
    assert_eq!(send(wrong).unwrap_err(), DENIED);
    let mut other = event("a", "tblA");
    other["event"]["file_token"] = json!("other");
    send(other).unwrap();
    send(event("a", "tblMissing")).unwrap();
    assert!(rx.try_recv().is_err());
    send(event("a", "tblA")).unwrap();
    send(event("b", "tblA")).unwrap();
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err());
    send(event("a", "tblA")).unwrap();
    assert!(rx.try_recv().is_err());
}
#[test]
fn encrypted_delivery_and_encrypted_challenge_interoperate_with_external_vector() {
    // Fixtures from Node's crypto AES-256-CBC and SHA-256, independent of the decoder.
    let raw=br#"{"encrypt":"AAECAwQFBgcICQoLDA0OD/vsAJkfXuApR8BkWmnh3WJ6KzEHPIFynuQ7zMpozkgShHrHBzxuPeSlwlL1rG8MFJIq6SMaQmHUv3UZ2BSoxf8wZnM1+IvcGPIyaBMKJixjf9LDUQaYD7zvvcIscjE/9UPDCorK9/3JJGx+BYFJ0VO+d5YPV4UhnabgaXmnMkRs1RIbAZhAL06etF/69I7kcq8wl8foqjRiDD1FlB9i3wkcOK/UETZCxCmPofCvAabGezzyZw8UE7vGEXogXZZmag=="}"#;
    let challenge=br#"{"encrypt":"AAECAwQFBgcICQoLDA0OD5W/uHKc/xbbEd5aKpu4eRTuwckfE6uMxLPY0SiFYWWWRhUKlkOZ7VWXZhIxLcxNeUvpedTJaHRCVX73NVSz15GRecAeMWX0wowWwRep4Rpg"}"#;
    let (cfg, rx) = config(Some("demo-encryption-key"));
    let now = 1_234_567_890;
    let mut headers = HeaderMap::new();
    headers.insert("x-lark-request-timestamp", "1234567890".parse().unwrap());
    headers.insert("x-lark-request-nonce", "demo-nonce".parse().unwrap());
    assert_eq!(
        process(&cfg, &headers, challenge, now).unwrap().0["challenge"],
        "hello"
    );
    assert_eq!(process(&cfg, &headers, raw, now).unwrap_err(), DENIED);
    headers.insert(
        "x-lark-signature",
        "3d2875d0272e0b072ea7ac1ca49354fa6429732212cc3bb7ab5b479d95e9ecf2"
            .parse()
            .unwrap(),
    );
    assert_eq!(process(&cfg, &headers, raw, now + 301).unwrap_err(), DENIED);
    assert!(rx.try_recv().is_err());
    assert!(process(&cfg, &headers, raw, now).is_ok());
    assert!(rx.try_recv().is_ok());
    assert_eq!(process(&cfg, &headers, challenge, now).unwrap_err(), DENIED);
    assert_eq!(
        process(
            &cfg,
            &HeaderMap::new(),
            event("c", "tblA").to_string().as_bytes(),
            now
        )
        .unwrap_err(),
        DENIED
    );
}
#[tokio::test]
async fn callback_server_exposes_one_size_limited_route() {
    let (cfg, _) = config(None);
    let server = router(cfg);
    let oversized = Request::post("/feishu/events")
        .body(Body::from(vec![0; MAX_PAYLOAD + 1]))
        .unwrap();
    assert_eq!(
        server.clone().oneshot(oversized).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let admin = Request::post("/v1/admin/changes")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        server.oneshot(admin).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}
