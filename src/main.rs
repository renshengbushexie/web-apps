mod engine;
mod http;

use std::{env, path::Path, sync::Arc};

use anyhow::Context;

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let credentials = http::Credentials::parse(
        &env::var("KB_ADMIN_TOKEN").context("KB_ADMIN_TOKEN is required")?,
        &env::var("KB_SHOP_TOKENS").unwrap_or_else(|_| "{}".into()),
        &env::var("KB_SHOP_TOKEN_HASHES").unwrap_or_else(|_| "{}".into()),
    )?;
    let ai = http::AiProvider::from_env()?;
    let database = env::var("KB_DB").unwrap_or_else(|_| "knowledge.sqlite3".into());
    let bind = env::var("KB_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let engine = Arc::new(engine::Engine::open(Path::new(&database))?);
    let app = http::router(engine, credentials, ai);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .context("bind knowledge API")?;
    eprintln!("Knowledge API listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate =
            signal(SignalKind::terminate()).expect("install SIGTERM signal handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
