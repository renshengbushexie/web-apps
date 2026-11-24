use super::{auth::Auth, knowledge};
use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use std::{fmt::Write, sync::Arc};

#[derive(Default, Deserialize)]
pub(super) struct Filters {
    table: Option<String>,
    by: Option<String>,
    value: Option<String>,
    cursor: Option<String>,
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn path(pairs: &[(&str, Option<&str>)]) -> String {
    let mut url = Url::parse("https://localhost/").expect("static origin");
    url.query_pairs_mut()
        .extend_pairs(pairs.iter().filter_map(|(k, v)| Some((*k, (*v)?))));
    escape(&format!("/?{}", url.query().unwrap_or_default()))
}

fn document(body: &str, logged_in: bool) -> Html<String> {
    let account = if logged_in {
        r#"<form action="/auth/logout" method="post"><button class="quiet">退出登录</button></form>"#
    } else {
        ""
    };
    Html(format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>客服知识库</title><link rel="stylesheet" href="/workbench.css"></head><body><header><div class="bar"><a class="brand" href="/">客服知识库</a>{account}</div></header><main>{body}</main></body></html>"#
    ))
}

fn landing(enabled: bool) -> Html<String> {
    let action = if enabled {
        r#"<a class="button" href="/auth/feishu/login">用飞书账号登录</a>"#
    } else {
        r#"<div class="notice"><strong>工作台登录待启用</strong><p>企业应用管理员配置飞书登录后，即可在这里查询。已有原表链接的同事可直接在飞书打开。</p></div>"#
    };
    document(
        &format!(
            r#"<section class="intro"><h1>商品知识查询</h1><p class="lead">用自己的飞书账号查找商品 SKU、参数和客服话术。可见内容由飞书原表权限决定。</p>{action}</section>"#
        ),
        false,
    )
}

fn error(status: StatusCode, logged_in: bool) -> Response {
    let message = match status {
        StatusCode::UNAUTHORIZED => "飞书登录已过期，请重新登录。",
        StatusCode::FORBIDDEN => "当前账号无权读取这份资料。如需访问，请联系飞书原库管理员。",
        StatusCode::NOT_FOUND => "当前账号可访问的知识表中未找到此表。",
        StatusCode::BAD_REQUEST => "筛选条件无效，请重新选择字段和值。",
        StatusCode::TOO_MANY_REQUESTS => "飞书请求较多，请稍后重试。",
        StatusCode::SERVICE_UNAVAILABLE => "知识库连接尚待管理员配置。",
        _ => "暂时无法读取飞书，请稍后重试。",
    };
    let link = if status == StatusCode::UNAUTHORIZED {
        r#"<a class="button" href="/auth/feishu/login">重新登录飞书</a>"#
    } else {
        r#"<a href="/">返回知识表</a>"#
    };
    (
        status,
        document(
            &format!("<section class=\"intro\"><h1>查询提示</h1><p>{message}</p>{link}</section>"),
            logged_in,
        ),
    )
        .into_response()
}

pub(super) async fn home(
    State(auth): State<Arc<Auth>>,
    headers: HeaderMap,
    q: Result<Query<Filters>, QueryRejection>,
) -> Response {
    if auth.token(&headers).is_none() {
        return landing(auth.enabled()).into_response();
    }
    let Ok(Query(q)) = q else {
        return error(StatusCode::BAD_REQUEST, true);
    };
    let value = q.value.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let (op, field) = if value.is_some() {
        let Some((op, field)) = q.by.as_deref().and_then(|s| s.split_once(':')) else {
            return error(StatusCode::BAD_REQUEST, true);
        };
        (Some(op), Some(field))
    } else {
        (None, None)
    };
    match knowledge::view(
        &auth,
        &headers,
        q.table.as_deref(),
        field,
        value,
        op,
        q.cursor.as_deref(),
    )
    .await
    {
        Ok(data) => document(&workbench(&data, &q), true).into_response(),
        Err((status, _)) => error(status, true),
    }
}

fn items<'a>(data: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    data[key].as_array().into_iter().flatten()
}
fn source_link(s: &str) -> Option<String> {
    let u = Url::parse(s).ok()?;
    (u.scheme() == "https"
        && u.host_str()
            .is_some_and(|host| host.ends_with(".feishu.cn"))
        && u.username().is_empty()
        && u.password().is_none()
        && u.port().is_none()
        && u.path().starts_with("/base/"))
    .then(|| escape(u.as_str()))
}
fn workbench(data: &Value, q: &Filters) -> String {
    let table = q.table.as_deref();
    let mut html = String::from("<h1>商品知识查询</h1><nav aria-label=\"知识表\">");
    for t in items(data, "tables") {
        let (Some(id), Some(name)) = (t["table_id"].as_str(), t["name"].as_str()) else {
            continue;
        };
        let current = if table == Some(id) {
            " aria-current=\"page\""
        } else {
            ""
        };
        write!(
            html,
            r#"<a href="{}"{current}>{}</a>"#,
            path(&[("table", Some(id))]),
            escape(name)
        )
        .unwrap();
    }
    html.push_str("</nav>");
    let Some(table) = table else {
        html.push_str(if items(data, "tables").next().is_some() {
            "<p>选择知识表，查看记录或按字段查询。</p>"
        } else {
            "<p>当前账号没有可访问的商品知识表。</p>"
        });
        return html;
    };
    let mut options = String::new();
    for f in items(data, "fields") {
        let Some(name) = f["name"].as_str() else {
            continue;
        };
        for op in items(f, "operators").filter_map(Value::as_str) {
            let key = format!("{op}:{name}");
            let selected = if q.by.as_ref() == Some(&key) {
                " selected"
            } else {
                ""
            };
            let label = if op == "is" { "等于" } else { "包含" };
            write!(
                options,
                r#"<option value="{}"{selected}>{} · {label}</option>"#,
                escape(&key),
                escape(name)
            )
            .unwrap();
        }
    }
    if !options.is_empty() {
        write!(html,r#"<form class="filter" method="get" action="/"><input type="hidden" name="table" value="{}"><label>筛选字段<select name="by">{options}</select></label><label>查询内容<input name="value" maxlength="512" value="{}" placeholder="输入商品 ID、型号或关键词"></label><button>查询</button><a href="{}">清除</a></form>"#,escape(table),escape(q.value.as_deref().unwrap_or("")),path(&[("table",Some(table))])).unwrap();
    }
    if items(data, "records").next().is_none() {
        html.push_str("<p>没有找到符合条件的记录。</p>");
    }
    for row in items(data, "records") {
        html.push_str("<article><div class=\"rowhead\"><h2>原表记录</h2>");
        if let Some(link) = row["source"].as_str().and_then(source_link) {
            write!(html, r#"<a href="{link}" rel="noreferrer">在飞书打开</a>"#).unwrap();
        }
        html.push_str("</div><dl>");
        if let Some(fields) = row["fields"].as_object() {
            for (name, v) in fields {
                if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
                    write!(
                        html,
                        "<div><dt>{}</dt><dd>{}</dd></div>",
                        escape(name),
                        escape(s)
                    )
                    .unwrap();
                }
            }
        }
        html.push_str("</dl></article>");
    }
    if let Some(next) = data["next_page_token"]
        .as_str()
        .filter(|_| data["has_more"] == true)
    {
        write!(
            html,
            r#"<a class="button" href="{}">下一页</a>"#,
            path(&[
                ("table", Some(table)),
                ("by", q.by.as_deref()),
                ("value", q.value.as_deref()),
                ("cursor", Some(next))
            ])
        )
        .unwrap();
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_only_safe_record_text_and_links() {
        let q = Filters {
            table: Some("tbl1".into()),
            ..Default::default()
        };
        let data = json!({"tables":[{"table_id":"tbl1","name":"<test>"}],"fields":[],"records":[{"fields":{"a":"<script>"},"source":"https://www.feishu.cn.evil.invalid/base/x"}],"has_more":true,"next_page_token":"a&b"});
        let html = workbench(&data, &q);
        assert!(
            html.contains("&lt;script&gt;")
                && html.contains("&lt;test&gt;")
                && html.contains("a%26b")
        );
        assert!(!html.contains("evil.invalid"));
        assert!(source_link("https://tenant.feishu.cn/base/example?table=tbl1&record=r").is_some());
    }
}
