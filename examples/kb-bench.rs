//! Local HTTP smoke driver. See docs/perf.md for the disposable service setup.
use anyhow::{Context, Result, ensure};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, env, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::{Instant, sleep_until},
};

const PRODUCTS: usize = 2_000;
const SLOTS: usize = 10;
const TOPICS: [&str; 6] = [
    "退款流程",
    "运费说明",
    "充电时间",
    "开票抬头",
    "保修期限",
    "配送范围",
];
const ANSWERS: [&str; 6] = [
    "请在订单详情选择申请售后，填写退货原因并保留物流单号，店铺将按订单进度处理。",
    "请以结算页展示的运费为准，多个商品合并发货时可先联系本店客服核对。",
    "建议使用随附的数据线与适配器，首次使用前请先查看说明书中的充电提示。",
    "申请开票时请核对抬头、税号和接收邮箱，提交后可以在订单详情查询进度。",
    "保修从签收之日开始计算，请保留购买凭证与机身序列号供售后人员核验。",
    "配送区域与预计送达日期取决于具体地址，结算前可以在商品页面查看。",
];

#[derive(Clone)]
struct Target {
    http: Client,
    base: String,
    admin: String,
    shops: Vec<String>,
}

fn shop_for(product: usize) -> String {
    format!("smoke-{:02}", product % 10)
}
fn product_id(product: usize) -> String {
    format!("0009007199254740993{product:04}")
}
fn record_id(product: usize, slot: usize) -> String {
    format!("rec-{product:04}-{slot}")
}
fn sku(slot: usize) -> String {
    format!("S-{slot:02}")
}

fn changes(revision: usize) -> Value {
    let mut rows = Vec::new();
    for p in 0..PRODUCTS {
        if revision > 1 && !(p / 10).is_multiple_of(10) {
            continue;
        }
        let shop = shop_for(p);
        let product = product_id(p);
        for slot in 0..SLOTS {
            let id = record_id(p, slot);
            let topic = slot.saturating_sub(4);
            let (kind, question, text) = if slot < 4 {
                (
                    "sku",
                    format!("商品 {p} 的配置与适配说明"),
                    format!(
                        "{}，颜色及配置以本型号为准，发货前请核对订单规格。",
                        sku(slot)
                    ),
                )
            } else {
                (
                    "answer",
                    format!("关于本商品的{}，下单前需要了解什么？", TOPICS[topic]),
                    ANSWERS[topic].to_owned(),
                )
            };
            rows.push(json!({"shop":shop,"id":id,"item":{
                "shop":shop,"id":id,"kind":kind,"scope":"product","product_ids":[product],"enabled":true,
                "sku":if slot < 4 {sku(slot)} else {String::new()}, "model":format!("样例型号 {}",p % 27),
                "question":question,"answer":format!("{text}【v{revision}】"),"source":""
            }}));
        }
    }
    json!({"changes":rows})
}

impl Target {
    async fn publish(&self, payload: Vec<u8>, revision: usize) -> Result<()> {
        let response: Value = self
            .http
            .post(format!("{}/v1/admin/changes", self.base))
            .bearer_auth(&self.admin)
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(120))
            .body(payload)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            response["version"].as_u64() == Some(revision as u64),
            "unexpected publication version"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
struct Response {
    version: usize,
    items: Vec<Row>,
}
#[derive(Deserialize)]
struct Row {
    id: String,
    answer: String,
}

struct Sample {
    kind: &'static str,
    done: Instant,
    scheduled_us: u64,
    lag_us: u64,
    bytes: usize,
    outcome: String,
    version: Option<usize>,
}

fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

async fn query(
    target: &Target,
    seq: usize,
    kind: &str,
    revision: usize,
) -> (String, usize, Option<usize>) {
    let random = mix(seq as u64 + 1);
    let p = (random as usize) % PRODUCTS;
    let slot = if kind == "sku" {
        ((random >> 32) % 4) as usize
    } else {
        4 + ((random >> 32) % 6) as usize
    };
    let product = product_id(p);
    let request = if kind == "sku" {
        target
            .http
            .get(format!("{}/v1/sku", target.base))
            .query(&[("product_id", product), ("sku", sku(slot))])
    } else {
        target
            .http
            .get(format!("{}/v1/search", target.base))
            .query(&[
                ("product_id", product.as_str()),
                ("q", TOPICS[slot - 4]),
                ("limit", "3"),
            ])
    };
    let response = match request.bearer_auth(&target.shops[p % 10]).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                if e.is_timeout() {
                    "timeout"
                } else {
                    "transport"
                }
                .into(),
                0,
                None,
            );
        }
    };
    let code = response.status();
    let Ok(body) = response.bytes().await else {
        return ("body_error".into(), 0, None);
    };
    let bytes = body.len();
    if !code.is_success() {
        return (code.as_u16().to_string(), bytes, None);
    }
    let Ok(response) = serde_json::from_slice::<Response>(&body) else {
        return ("invalid_json".into(), bytes, None);
    };
    let v = response.version;
    let expected = record_id(p, slot);
    let expected_revision = if (p / 10).is_multiple_of(10) { v } else { 1 };
    let marker = format!("【v{expected_revision}】");
    let prefix = format!("rec-{p:04}-");
    let valid = (v == revision || v + 1 == revision)
        && response.items.iter().any(|r| r.id == expected)
        && response
            .items
            .iter()
            .all(|r| r.id.starts_with(&prefix) && r.answer.ends_with(&marker));
    (
        (if valid { "ok" } else { "unexpected_response" }).into(),
        bytes,
        Some(v),
    )
}

fn percentiles(mut values: Vec<u64>) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_unstable();
    let p = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1] as f64 / 1000.;
    json!({"p50":p(50), "p95":p(95), "p99":p(99)})
}

fn metrics(samples: &[&Sample]) -> Value {
    let mut outcomes = BTreeMap::new();
    let mut versions = BTreeMap::new();
    for s in samples {
        *outcomes.entry(s.outcome.as_str()).or_insert(0) += 1;
        if let Some(v) = s.version {
            *versions.entry(v).or_insert(0) += 1;
        }
    }
    json!({
        "arrivals":samples.len(),"outcomes":outcomes,"versions":versions,
        "response_body_bytes":samples.iter().map(|s| s.bytes).sum::<usize>(),
        "planned_arrival_to_body_ms":percentiles(samples.iter().map(|s| s.scheduled_us).collect()),
        "generator_start_lag_ms":percentiles(samples.iter().map(|s| s.lag_us).collect()),
        "dispatch_to_checked_body_ms":percentiles(samples.iter().map(|s| s.scheduled_us - s.lag_us).collect())
    })
}

async fn phase(target: &Target, qps: usize, seconds: usize, revision: usize) -> Result<Value> {
    let payload = serde_json::to_vec(&changes(revision))?;
    let start = Instant::now() + Duration::from_millis(300);
    let publication = {
        let target = target.clone();
        tokio::spawn(async move {
            sleep_until(start + Duration::from_secs((seconds / 2) as u64)).await;
            let before = Instant::now();
            target.publish(payload, revision).await?;
            Ok::<_, anyhow::Error>((before, Instant::now()))
        })
    };
    let permits = Arc::new(Semaphore::new(1024));
    let mut queries = JoinSet::new();
    let mut samples = Vec::with_capacity(qps * seconds);
    let (arrivals, mut schedule) = mpsc::unbounded_channel();
    let timer = tokio::task::spawn_blocking(move || {
        for seq in 0..qps * seconds {
            let due = start + Duration::from_nanos(seq as u64 * 1_000_000_000 / qps as u64);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            if arrivals.send((seq, due)).is_err() {
                return;
            }
        }
    });
    while let Some((seq, due)) = schedule.recv().await {
        let target = target.clone();
        let permit = permits.clone().try_acquire_owned();
        let kind = if seq % 10 < 6 { "sku" } else { "search" };
        queries.spawn(async move {
            let sent = Instant::now();
            let (outcome, bytes, version) = if let Ok(_permit) = permit {
                query(&target, seq, kind, revision).await
            } else {
                ("generator_overload".into(), 0, None)
            };
            let done = Instant::now();
            Sample {
                kind,
                done,
                outcome,
                bytes,
                version,
                scheduled_us: (done - due).as_micros() as u64,
                lag_us: (sent - due).as_micros() as u64,
            }
        });
        while let Some(result) = queries.try_join_next() {
            samples.push(result?);
        }
    }
    timer.await?;
    while let Some(result) = queries.join_next().await {
        samples.push(result?);
    }
    let (before, after) = publication.await??;
    let all: Vec<_> = samples.iter().collect();
    let sku: Vec<_> = samples.iter().filter(|s| s.kind == "sku").collect();
    let search: Vec<_> = samples.iter().filter(|s| s.kind == "search").collect();
    let publish: Vec<_> = samples
        .iter()
        .filter(|s| s.done >= before && s.done <= after)
        .collect();
    Ok(
        json!({"qps":qps,"duration_s":seconds,"all":metrics(&all),"sku":metrics(&sku),
        "search":metrics(&search),"during_publication":metrics(&publish),
        "publication_ms":(after-before).as_secs_f64()*1000., "changed_rows":2_000}),
    )
}

#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    let base =
        env::var("KB_BENCH_URL").context("set KB_BENCH_URL to the disposable local server")?;
    let url = reqwest::Url::parse(&base)?;
    ensure!(
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
            && url.path() == "/",
        "bench only accepts a loopback HTTP URL"
    );
    let admin = env::var("KB_BENCH_ADMIN")?;
    let prefix = env::var("KB_BENCH_SHOP_PREFIX")?;
    let shops = (0..10).map(|p| format!("{prefix}-{p}")).collect();
    let target = Target {
        http: Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?,
        base: base.trim_end_matches('/').to_owned(),
        admin,
        shops,
    };
    let health: Value = target
        .http
        .get(format!("{}/healthz", target.base))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        health["version"].as_u64() == Some(0),
        "benchmark requires a fresh empty database"
    );
    let seconds: usize = env::var("KB_BENCH_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse()?;
    ensure!(
        (4..=600).contains(&seconds),
        "duration out of range (4 to 600 s)"
    );
    let payload = serde_json::to_vec(&changes(1))?;
    let seed_bytes = payload.len();
    let seed_start = Instant::now();
    target.publish(payload, 1).await?;
    println!(
        "{}",
        json!({"seed_records":PRODUCTS*SLOTS,"seed_bytes":seed_bytes,"seed_s":seed_start.elapsed().as_secs_f64()})
    );
    for seq in 0..100 {
        let kind = if seq % 2 == 0 { "sku" } else { "search" };
        ensure!(
            query(&target, seq, kind, 1).await.0 == "ok",
            "warmup failed"
        );
    }
    let mut errors = 0;
    for (qps, revision) in [(100, 2), (300, 3)] {
        let result = phase(&target, qps, seconds, revision).await?;
        let received = result["all"]["outcomes"]["ok"].as_u64().unwrap_or(0) as usize;
        errors += qps * seconds - received;
        println!("{result}");
    }
    ensure!(errors == 0, "{errors} unsuccessful read(s)");
    Ok(())
}
