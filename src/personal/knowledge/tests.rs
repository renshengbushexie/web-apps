use super::*;
use axum::{body::to_bytes, extract::Path, http::Request as HttpRequest, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn human(h: &HeaderMap) -> &str {
    h.get(header::AUTHORIZATION)
        .and_then(|s| s.to_str().ok())
        .unwrap_or("")
}
fn reply(items: Value) -> Json<Value> {
    Json(json!({"code":0,"data":{"items":items,"has_more":false}}))
}
async fn setup() -> (Router, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let counts = Arc::new(AtomicUsize::new(0));
    let app=Router::new()
        .route("/tables",get(|h:HeaderMap,q:Query<BTreeMap<String,String>>|async move{
            if human(&h)=="Bearer revoked" {return Json(json!({"code":1254302}));}
            if human(&h)=="Bearer A" && !q.contains_key("page_token"){
                return Json(json!({"code":0,"data":{"items":[{"table_id":"tblPrivate","name":"其他数据"}],"has_more":true,"page_token":"two"}}));
            }
            let mut visible=vec![json!({"table_id":"tblSku","name":"SKU 明细"})];
            if human(&h)=="Bearer A" { visible.push(json!({"table_id":"tblAnswers","name":"商品 QA"}));}
            reply(json!(visible))
        }))
        .route("/tables/{id}/fields",get(|Path(_):Path<String>,h:HeaderMap|async move{
            let mut list=vec![json!({"field_name":"名字","field_id":"fldName","type":1}),json!({"field_name":"编号","field_id":"fldNum","type":2})];
            if human(&h)=="Bearer A"{list.push(json!({"field_name":"内部备注","field_id":"fldPrivate","type":1}));}
            reply(json!(list))
        }))
        .route("/tables/{id}/records/search",post(|State(count):State<Arc<AtomicUsize>>,h:HeaderMap,q:Query<BTreeMap<String,String>>,Json(body):Json<Value>|async move {
            count.fetch_add(1,Ordering::SeqCst);
            assert_eq!(q.get("page_size").unwrap(),"10");
            if body["filter"].is_object(){
                assert_eq!(body["filter"]["conditions"][0],json!({"field_name":"编号","operator":"is","value":["12345678901234567890"]}));
            }
            let who=human(&h); let name=if who=="Bearer A"{"Alice"}else{"Bob"};
            reply(json!([{"record_id":"rec123","fields":{"名字":[{"text":name}],"内部备注":"private-A","未映射":"ignored","编号":12345678901234567890u64}}]))
        })).with_state(counts.clone());
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/tables", socket.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(socket, app).await.unwrap();
    });
    let state = Knowledge {
        session: Arc::new(|h| {
            h.get(header::COOKIE)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.strip_prefix("test="))
                .map(Arc::from)
        }),
        source: Some(Source {
            base: "appDemo".into(),
            web_origin: Some("https://demo.feishu.cn".into()),
            root,
            client: Client::builder().no_proxy().build().unwrap(),
        }),
    };
    (routes(state), counts, server)
}
async fn call(server: &Router, url: &str, session: Option<&str>) -> (StatusCode, Value) {
    let mut req = HttpRequest::get(url);
    if let Some(user) = session {
        req = req.header(header::COOKIE, format!("test={user}"));
    }
    let response = server
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn sessions_only_see_their_own_tables_fields_and_records() {
    let (app, count, task) = setup().await;
    assert_eq!(
        call(&app, "/me/tables", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "/me/tables", Some("revoked")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, "/me/tables", Some("A")).await.1["tables"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        call(&app, "/me/tables", Some("B")).await.1["tables"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        call(&app, "/me/records?table_id=tblPrivate", Some("A"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "/me/records?table_id=tblAnswers", Some("B"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let url = "/me/records?table_id=tblSku&field=%E7%BC%96%E5%8F%B7&value=12345678901234567890";
    let (sa, a) = call(&app, url, Some("A")).await;
    let (sb, b) = call(&app, url, Some("B")).await;
    assert_eq!((sa, sb), (StatusCode::OK, StatusCode::OK));
    let ra = &a["records"][0];
    let rb = &b["records"][0];
    assert_eq!(ra["fields"]["名字"], "Alice");
    assert_eq!(rb["fields"]["名字"], "Bob");
    assert_eq!(ra["fields"]["内部备注"], "private-A");
    assert!(rb["fields"].get("内部备注").is_none());
    assert!(ra["fields"].get("未映射").is_none());
    assert_eq!(rb["fields"]["编号"], "12345678901234567890");
    assert_eq!(
        rb["source"],
        "https://demo.feishu.cn/base/appDemo?table=tblSku&record=rec123"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let (s, _) = call(
        &app,
        "/me/records?table_id=tblSku&field=%E7%BC%96%E5%8F%B7&value=4&op=contains",
        Some("B"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(
        call(&app, "/me/records?table_id=tblSku&limit=26", Some("A"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    task.abort();
}

#[test]
fn links_are_scoped_to_the_deployed_tenant() {
    assert_eq!(
        tenant_origin("https://demo.feishu.cn/").as_deref(),
        Some("https://demo.feishu.cn")
    );
    for bad in [
        "https://name@demo.feishu.cn",
        "https://demo.feishu.cn:443",
        "https://demo.feishu.cn/else",
        "https://fake.feishu.cn.other.test",
        "http://demo.feishu.cn",
        "https://.feishu.cn",
    ] {
        assert!(tenant_origin(bad).is_none());
    }
    let raw = "https://demo.feishu.cn/base/appDemo?table=tblA&view=vewGood&record=recA";
    assert!(record_link(None, "appDemo", "tblA", "recA", Some(raw)).is_none());
    assert_eq!(
        record_link(
            Some("https://demo.feishu.cn"),
            "appDemo",
            "tblA",
            "recA",
            Some(raw)
        )
        .as_deref(),
        Some(raw)
    );
    assert_eq!(
        record_link(
            Some("https://demo.feishu.cn"),
            "appDemo",
            "tblA",
            "recA",
            Some("https://other.test/")
        )
        .as_deref(),
        Some("https://demo.feishu.cn/base/appDemo?table=tblA&record=recA")
    );
}
#[tokio::test]
async fn missing_configuration_leaves_auth_boundary_intact() {
    let app = routes(Knowledge {
        session: Arc::new(|h| h.contains_key(header::COOKIE).then(|| Arc::from("dummy"))),
        source: None,
    });
    assert_eq!(
        call(&app, "/me/tables", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "/me/tables", Some("demo")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
