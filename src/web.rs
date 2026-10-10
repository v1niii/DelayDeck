use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;

use crate::config::{Patch, Service};
use crate::output::parse_url;
use crate::state::App;
use crate::update;

const PANEL: &str = include_str!("../web/index.html");
const OVERLAY: &str = include_str!("../web/overlay.html");
const PLAYER: &str = include_str!("../web/vendor/mpegts.js");

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { page(PANEL) }))
        .route("/overlay", get(|| async { page(OVERLAY) }))
        .route("/preview.flv", get(preview))
        .route(
            "/vendor/mpegts.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "max-age=86400")], PLAYER) }),
        )
        .route("/api/status", get(status))
        .route("/api/config", get(config).post(update_config))
        .route("/api/events", get(events))
        .route("/api/delay", post(set_delay))
        .route("/api/skip", post(skip))
        .route("/api/update/check", post(check_update))
        .route("/api/update/install", post(install_update))
        .layer(middleware::from_fn(guard))
        .with_state(app)
}

async fn check_update(State(app): State<Arc<App>>) -> Response {
    update::check(&app, true).await;
    json(app.status_tx.borrow().clone())
}

async fn install_update(State(app): State<Arc<App>>) -> Response {
    if app.is_streaming() {
        let msg = "Stop streaming in OBS first: updating restarts DelayDeck";
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "error": msg }))).into_response();
    }
    match update::install(&app).await {
        Ok(()) => Json(serde_json::json!({ "restarting": true })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": e }))).into_response(),
    }
}

fn page(html: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response()
}

fn json(body: Arc<str>) -> Response {
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body.to_string()).into_response()
}

async fn preview(State(app): State<Arc<App>>) -> Response {
    let body = axum::body::Body::from_stream(app.preview.flv());
    ([(header::CONTENT_TYPE, "video/x-flv"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

async fn guard(req: Request, next: Next) -> Response {
    let headers = req.headers();
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    if text(header::HOST).is_some_and(|h| !local_authority(h)) {
        return (StatusCode::FORBIDDEN, "DelayDeck only answers local requests").into_response();
    }
    if req.method() == Method::POST {
        let origin_ok = text(header::ORIGIN).is_none_or(|o| o.split_once("://").is_some_and(|(_, a)| local_authority(a)));
        let is_json = text(header::CONTENT_TYPE).is_some_and(|c| c.starts_with("application/json"));
        if !origin_ok {
            return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
        }
        if !is_json {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "send JSON (Content-Type: application/json)").into_response();
        }
    }
    next.run(req).await
}

fn local_authority(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok()
}

async fn status(State(app): State<Arc<App>>) -> Response {
    json(app.status_tx.borrow().clone())
}

async fn config(State(app): State<Arc<App>>) -> Response {
    json(app.config_tx.borrow().clone())
}

async fn update_config(State(app): State<Arc<App>>, Json(patch): Json<Patch>) -> Response {
    let service = patch.service.unwrap_or(app.config().service);
    if service == Service::Custom {
        let url = patch.custom_url.clone().unwrap_or_else(|| app.config().custom_url);
        if !url.trim().is_empty()
            && let Err(e) = parse_url(&url) {
                return bad_request(format!("Invalid server URL: {e}"));
            }
    }
    app.apply_patch(patch);
    json(app.config_tx.borrow().clone())
}

#[derive(Deserialize)]
struct DelayRequest {
    seconds: Option<f64>,
    delta: Option<f64>,
}

async fn set_delay(State(app): State<Arc<App>>, Json(req): Json<DelayRequest>) -> Response {
    let secs = match (req.seconds, req.delta) {
        (Some(s), _) => s,
        (None, Some(d)) => app.config().delay + d,
        (None, None) => return bad_request("expected {\"seconds\": n} or {\"delta\": n}"),
    };
    if !secs.is_finite() {
        return bad_request("delay must be a number");
    }
    app.set_delay(secs);
    json(app.status_tx.borrow().clone())
}

async fn skip(State(app): State<Arc<App>>) -> Response {
    app.set_delay(0.0);
    json(app.status_tx.borrow().clone())
}

#[derive(Deserialize)]
struct EventsQuery {
    topics: Option<String>,
}

async fn events(State(app): State<Arc<App>>, Query(q): Query<EventsQuery>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let topics = q.topics.unwrap_or_else(|| "status,config,log".into());
    let wants = |t: &str| topics.split(',').any(|x| x.trim() == t);
    let mut feeds = Vec::new();
    if wants("status") {
        feeds.push(feed(app.status_tx.subscribe(), "status").boxed());
    }
    if wants("config") {
        feeds.push(feed(app.config_tx.subscribe(), "config").boxed());
    }
    if wants("log") {
        feeds.push(feed(app.log_tx.subscribe(), "log").boxed());
    }
    Sse::new(stream::select_all(feeds)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn feed(rx: watch::Receiver<Arc<str>>, name: &'static str) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    stream::unfold((rx, true), move |(mut rx, first)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let data = rx.borrow_and_update().clone();
        Some((Ok(Event::default().event(name).data(&*data)), (rx, false)))
    })
}

#[cfg(test)]
mod tests {
    use super::local_authority;

    #[test]
    fn host_check() {
        assert!(local_authority("127.0.0.1:3000"));
        assert!(local_authority("localhost:3000"));
        assert!(local_authority("[::1]:3000"));
        assert!(local_authority("192.168.1.20:3000"));
        assert!(!local_authority("evil.example.com:3000"));
        assert!(!local_authority("127.0.0.1.nip.io"));
    }
}
