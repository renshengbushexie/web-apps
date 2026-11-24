mod auth;
mod knowledge;
mod page;

use axum::{
    Router,
    extract::Request,
    http::{HeaderValue, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use std::sync::Arc;

pub fn router() -> anyhow::Result<Router> {
    let auth = Arc::new(auth::Auth::from_env()?);
    with_auth(auth)
}

fn with_auth(auth: Arc<auth::Auth>) -> anyhow::Result<Router> {
    let api = knowledge::router(auth.clone())?;
    Ok(Router::new()
        .route("/", get(page::home))
        .route(
            "/workbench.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("workbench.css"),
                )
            }),
        )
        .route("/auth/feishu/login", get(auth::login))
        .route("/auth/feishu/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .with_state(auth)
        .merge(api)
        .layer(middleware::from_fn(protect)))
}

async fn protect(req: Request, next: Next) -> Response {
    // A same-origin form POST needs its Origin for logout validation.
    let home = req.uri().path() == "/";
    let mut res = next.run(req).await;
    let referrer = if home && res.status() == axum::http::StatusCode::OK {
        "same-origin"
    } else {
        "no-referrer"
    };
    for (name, value) in [
        (header::CACHE_CONTROL, "private, no-store"),
        (header::REFERRER_POLICY, referrer),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        res.headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    res
}
