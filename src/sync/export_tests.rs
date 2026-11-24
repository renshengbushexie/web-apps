use super::*;
use serde_json::json;

fn table() -> Table {
    serde_json::from_value(json!({"name":"qa","table_id":"tblDemo","kind":"answer",
        "fields":{"shop":"shop","id":"id","scope":"scope","product_ids":"product",
        "enabled":"status","question":"q","answer":"a","source":"link"}}))
    .unwrap()
}
const HEADERS: &str = "shop,id,scope,product,status,q,a,link\n";

#[test]
fn exports_preserve_zeroes_quoted_cells_source_and_explicit_disables() {
    let table = table();
    let link = table_link(Some("https://tenant.feishu.cn/"), "base", &table).unwrap();
    let csv = format!(
        "\u{feff}{HEADERS}001,002,product,00009,TRUE,Q,\"a,b\nanswer\",\n001,003,,,FALSE,,,\n001,004,shop,,启用,Q,A,https://docs.example.org/source"
    );
    let (items, disabled) = parse(csv.as_bytes(), &table, &link).unwrap();
    let item = &items[&("001".into(), "002".into())];
    assert_eq!(item["product_ids"], json!(["00009"]));
    assert_eq!(item["answer"], "a,b\nanswer");
    assert_eq!(
        item["source"],
        "https://tenant.feishu.cn/base/base?table=tblDemo"
    );
    assert_eq!(
        items[&("001".into(), "004".into())]["source"],
        "https://docs.example.org/source"
    );
    assert!(disabled.contains(&("001".into(), "003".into())));
    assert_eq!(table_link(None, "base", &table).unwrap(), "");
    assert!(
        table_link(
            Some("https://tenant.feishu.cn.evil.example"),
            "base",
            &table
        )
        .is_err()
    );
}

#[test]
fn ambiguous_or_incomplete_exports_reject_the_entire_file() {
    for tail in [
        "s,i,shop,,true,q,a,\"unterminated",
        "s,i,shop,,true,q,he\"llo,",
        "s,i,product,\"001,002\",true,q,a,",
    ] {
        assert!(parse(format!("{HEADERS}{tail}").as_bytes(), &table(), "").is_err());
    }
    for row in [
        "s,i,shop,,unknown,q,a,",
        "s,i,shop,,,q,a,",
        "s,i,,x,true,q,a,",
        "s,i,shop,,true,q,,",
        "s,i,product,,true,q,a,",
        "s,i,shop,,true,q,a,javascript:x",
        "s,i,shop,,true,q,a,\ns,i,,,false,,,",
        "s,i,shop,,true,q",
    ] {
        let csv = format!("{HEADERS}s,previous,shop,,true,q,a,\n{row}");
        assert!(parse(csv.as_bytes(), &table(), "").is_err(), "{row}");
    }
    for header in [
        "shop,id,scope,product,status,q,a",
        "shop,id,scope,product,status,q,a,link,id",
    ] {
        assert!(parse(header.as_bytes(), &table(), "").is_err());
    }
    let mut invalid = HEADERS.as_bytes().to_vec();
    invalid.push(0xff);
    assert!(parse(invalid.as_slice(), &table(), "").is_err());
    let (items, disabled) = parse(HEADERS.as_bytes(), &table(), "").unwrap();
    assert!(items.is_empty() && disabled.is_empty());
}
