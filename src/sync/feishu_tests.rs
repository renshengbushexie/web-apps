use super::*;
use crate::{import, mapping, state::State};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::Path,
    thread,
};

fn table() -> Table {
    serde_json::from_value(json!({"name":"faq","table_id":"tblA","kind":"answer",
        "fields":{"shop":"store","id":"source_id","scope":"scope","product_ids":"products","enabled":"active",
        "question":"raw_q","answer":"raw_a","answer_override":"approved","source":"url"}})).unwrap()
}
fn test_feishu(addr: std::net::SocketAddr) -> Feishu {
    Feishu {
        client: Client::builder().no_proxy().build().unwrap(),
        base: "appDemo".into(),
        app_id: "fake".into(),
        secret: "fake".into(),
        origin: format!("http://{addr}"),
    }
}
fn server(responses: Vec<Value>) -> (Feishu, thread::JoinHandle<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = test_feishu(listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for value in responses {
            let (mut conn, _) = listener.accept().unwrap();
            let (line, body) = {
                let mut reader = BufReader::new(&conn);
                let mut head = String::new();
                reader.read_line(&mut head).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, val)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = val.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                (head, String::from_utf8(bytes).unwrap())
            };
            requests.push((line, body));
            let json = value.to_string();
            write!(conn,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",json.len(),json).unwrap();
        }
        requests
    });
    (client, task)
}
fn page(items: Vec<Value>, total: usize, next: Option<&str>) -> Value {
    json!({"code":0,"data":{"items":items,"has_more":next.is_some(),"page_token":next,"total":total}})
}
fn row(id: &str) -> Value {
    json!({"record_id":format!("rec{id}"),"fields":{"store":"shop","source_id":id,"scope":"product","products":"[\"0007\",\"12345678901234567890\"]","active":true,"raw_q":"Q","raw_a":"A","approved":"Edited\nline 2","url":{"link":"https://example.org/s","text":"ref"}}})
}
#[test]
fn pages_complete_before_publication_and_identifiers_are_lossless() {
    let (client, task) = server(vec![
        page(vec![row("001")], 2, Some("next")),
        page(vec![row("002")], 2, None),
    ]);
    let table = table();
    let rows = client.list_records(&table, "t").unwrap();
    let snap = mapping::snapshot(&table, "appDemo", &rows).unwrap();
    let first = &snap[&("shop".into(), "001".into())];
    assert_eq!(first["answer"], "Edited\nline 2");
    assert_eq!(
        first["product_ids"],
        json!(["0007", "12345678901234567890"])
    );
    assert_eq!(first["source"], "https://example.org/s");
    assert!(task.join().unwrap()[1].0.contains("page_token=next"));
    let (client, task) = server(vec![
        page(vec![row("001")], 2, Some("next")),
        json!({"code":1254036}),
    ]);
    assert!(client.list_records(&table, "t").is_err());
    task.join().unwrap();
    let (client, task) = server(vec![page(vec![row("001")], 2, None)]);
    assert!(client.list_records(&table, "t").is_err());
    task.join().unwrap();
}
#[test]
fn uncertain_scope_status_or_duplicate_id_never_publishes() {
    let t = table();
    let mut r = row("001");
    r["fields"]["scope"] = json!("unknown");
    let snapshot = |values: Vec<Value>| {
        mapping::snapshot(
            &t,
            "base",
            &serde_json::from_value::<Vec<Record>>(json!(values)).unwrap(),
        )
    };
    assert!(snapshot(vec![r]).unwrap().is_empty());
    let mut r = row("001");
    r["fields"]["active"] = json!("pending");
    assert!(snapshot(vec![r]).unwrap().is_empty());
    assert!(snapshot(vec![row("001"), row("001")]).is_err());
    let mut r = row("001");
    r["fields"]["source_id"] = json!(42);
    assert!(snapshot(vec![r]).is_err());
}
#[test]
fn xiaoduo_upsert_uses_only_raw_columns_and_preserves_manual_edits() {
    let t = table();
    let schema = t
        .fields
        .names()
        .into_iter()
        .map(|n| json!({"field_name":n,"type":if n=="active"{7}else{1}}))
        .collect::<Vec<_>>();
    let (client, task) = server(vec![
        json!({"code":0,"tenant_access_token":"fake"}),
        page(schema.clone(), schema.len(), None),
        page(vec![row("001")], 1, None),
        json!({"code":0}),
        json!({"code":0}),
    ]);
    let file = std::env::temp_dir().join(format!("kb-sync-test-{}-import.csv", std::process::id()));
    std::fs::write(&file,"shop,id,scope,product_ids,question,answer\nshop,001,product,0007,Q,edited raw\nshop,002,shop,,Q,brand new\n").unwrap();
    let state = State::open(Path::new(":memory:")).unwrap();
    let result = import::run(&client, &t, file.to_str().unwrap(), &state);
    std::fs::remove_file(&file).unwrap();
    result.unwrap();
    let requests = task.join().unwrap();
    assert!(requests[3].0.starts_with("PUT ") && requests[3].0.contains("rec001"));
    assert!(requests[4].0.starts_with("POST ") && requests[4].0.contains("client_token="));
    for req in &requests[3..] {
        let fields: Value = serde_json::from_str(&req.1).unwrap();
        assert!(fields["fields"].get("approved").is_none());
        assert!(fields["fields"].get("active").is_none());
    }
    assert!(requests[3].1.contains("edited raw"));
    assert!(requests[4].1.contains("\"source_id\":\"002\""));
}
