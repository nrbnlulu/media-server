//! Internal import routes. Absent unless an operator configures an import token.
use crate::domain::dvr::import_jobs::{ImportManager, ImportRequest};
use axum::{
    Json, Router,
    extract::ws::Message,
    extract::{DefaultBodyLimit, Path, Request, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub fn router(manager: Arc<ImportManager>, token: String) -> Router {
    Router::new()
        .route("/imports", post(start))
        .route("/imports/ws", get(events))
        .route("/imports/:id", get(status).delete(cancel))
        .route("/prealarm/exists", post(exists))
        .route_layer(middleware::from_fn_with_state(Arc::new(token), authorize))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(manager)
}
async fn authorize(
    State(token): State<Arc<String>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let authorized = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|supplied| bool::from(supplied.as_bytes().ct_eq(token.as_bytes())));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}
async fn start(
    State(manager): State<Arc<ImportManager>>,
    Json(request): Json<ImportRequest>,
) -> Response {
    match manager.start(request).await {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}
async fn status(State(manager): State<Arc<ImportManager>>, Path(id): Path<String>) -> Response {
    match manager.get(&id) {
        Some(status) => Json(status).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn cancel(State(manager): State<Arc<ImportManager>>, Path(id): Path<String>) -> StatusCode {
    if manager.cancel(&id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
#[derive(Deserialize)]
struct ExistenceRequest {
    ids: Vec<String>,
}
#[derive(Serialize)]
struct ExistenceResponse {
    ids: Vec<String>,
}
async fn exists(
    State(manager): State<Arc<ImportManager>>,
    Json(request): Json<ExistenceRequest>,
) -> Json<ExistenceResponse> {
    Json(ExistenceResponse {
        ids: manager.exists(&request.ids),
    })
}
async fn events(State(manager): State<Arc<ImportManager>>, upgrade: WebSocketUpgrade) -> Response {
    let mut events = manager.subscribe();
    upgrade.on_upgrade(move |mut socket| async move {
        loop {
            tokio::select! {
                event=events.recv()=>{
                    let Ok(event)=event else {break;};
                    let Ok(json)=serde_json::to_string(&event) else {break;};
                    if socket.send(Message::Text(json)).await.is_err() {break;}
                }
                message=socket.recv()=>{
                    match message {
                        Some(Ok(Message::Ping(data)))=>{if socket.send(Message::Pong(data)).await.is_err() {break;}},
                        Some(Ok(Message::Close(_)))|Some(Err(_))|None=>break,
                        _=>{},
                    }
                }
            }
        }
    }).into_response()
}
