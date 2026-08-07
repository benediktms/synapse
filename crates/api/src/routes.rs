use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use domain::{Scope, Workspace};
use serde::Deserialize;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, MakeSpan, TraceLayer};
use tracing::Level;

use crate::backend::Backend;
use crate::dto::{
    ContextResponse, ExportDoc, GraphDto, HealthResponse, ImportReport, ListResponse, MemoryDto,
    MoveBody, MoveResponse, PatchMemoryBody, PutMemoryBody, PutPreferenceBody, SearchResponse,
    WorkspaceDto, WorkspacesResponse,
};
use crate::error::ApiError;
use crate::ops::{self, SearchArgs};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(600);
const BODY_LIMIT: usize = 32 * 1024 * 1024;
const UNREADY_PUBLIC_REASON: &str = "server is not ready; see server logs for detail";

pub struct AppState<B> {
    backend: B,
    token: Arc<str>,
}

impl<B: Clone> Clone for AppState<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            token: Arc::clone(&self.token),
        }
    }
}

pub fn router<B: Backend>(backend: B, token: &str) -> Router {
    let state = AppState {
        backend,
        token: token.into(),
    };
    let bulk = Router::new()
        .route("/import", axum::routing::post(import::<B>))
        .route(
            "/preferences/import",
            axum::routing::post(import_preferences::<B>),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            IMPORT_TIMEOUT,
        ));
    let data = Router::new()
        .route("/workspaces", get(get_workspaces::<B>))
        .route("/workspaces/{ws}", put(put_workspace::<B>))
        .route(
            "/memories/{id}",
            put(put_memory::<B>)
                .patch(patch_memory::<B>)
                .delete(delete_memory::<B>)
                .get(get_memory::<B>),
        )
        .route("/memories/{id}/move", axum::routing::post(move_memory::<B>))
        .route(
            "/memories/{id}/links",
            get(links::<B>)
                .post(create_link::<B>)
                .patch(retype_link::<B>)
                .delete(delete_link::<B>),
        )
        .route("/memories", get(list_memories::<B>))
        .route("/memories/search", get(search::<B>))
        .route(
            "/preferences/{id}",
            put(put_preference::<B>)
                .patch(patch_preference::<B>)
                .delete(delete_preference::<B>)
                .get(get_preference::<B>),
        )
        .route("/preferences", get(list_preferences::<B>))
        .route("/preferences/export", get(export_preferences::<B>))
        .route("/context", get(context::<B>))
        .route("/export", get(export::<B>))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .merge(bulk)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ready_gate::<B>,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), auth::<B>));
    Router::new()
        .route("/health", get(health::<B>))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .merge(data)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(PathOnlySpan)
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}

/// Records only the method and path: query strings carry recall queries and
/// headers carry the bearer token, and neither may reach the logs.
#[derive(Clone, Copy)]
struct PathOnlySpan;

impl<B> MakeSpan<B> for PathOnlySpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        tracing::info_span!(
            "request",
            method = %request.method(),
            path = %request.uri().path(),
            version = ?request.version(),
        )
    }
}

async fn auth<B: Backend>(
    State(state): State<AppState<B>>,
    request: Request,
    next: Next,
) -> Response {
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), state.token.as_bytes()));
    if authorized {
        next.run(request).await
    } else {
        ApiError::Unauthorized.into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn ready_gate<B: Backend>(
    State(state): State<AppState<B>>,
    request: Request,
    next: Next,
) -> Response {
    match state.backend.ready() {
        Ok(()) => next.run(request).await,
        Err(reason) => ApiError::Unready(reason).into_response(),
    }
}

async fn health<B: Backend>(State(state): State<AppState<B>>) -> Response {
    match state.backend.ready() {
        Ok(()) => Json(HealthResponse {
            status: "ready".into(),
            reason: None,
        })
        .into_response(),
        Err(reason) => {
            tracing::warn!(%reason, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(HealthResponse {
                    status: "unready".into(),
                    reason: Some(UNREADY_PUBLIC_REASON.into()),
                }),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct WsQuery {
    ws: Option<String>,
}

fn require_ws(query: &WsQuery) -> Result<Workspace, ApiError> {
    match query.ws.as_deref() {
        Some(name) => ops::parse_ws(name),
        None => Err(ApiError::BadRequest(
            "missing required query parameter: ws".into(),
        )),
    }
}

async fn put_workspace<B: Backend>(
    State(state): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<WorkspaceDto>), ApiError> {
    let (created, dto) = ops::create_workspace(&state.backend, &name).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(dto)))
}

async fn get_workspaces<B: Backend>(
    State(state): State<AppState<B>>,
) -> Result<Json<WorkspacesResponse>, ApiError> {
    Ok(Json(ops::workspaces(&state.backend).await?))
}

async fn save_into<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
    id: &str,
    body: PutMemoryBody,
) -> Result<(StatusCode, Json<MemoryDto>), ApiError> {
    let saved = ops::save(&state.backend, ws, id, body).await?;
    let status = if saved.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(saved.memory)))
}

async fn put_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    Json(body): Json<PutMemoryBody>,
) -> Result<(StatusCode, Json<MemoryDto>), ApiError> {
    let ws = require_ws(&query)?;
    save_into(&state, &ws, &id, body).await
}

async fn put_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Json(body): Json<PutPreferenceBody>,
) -> Result<(StatusCode, Json<MemoryDto>), ApiError> {
    let body = PutMemoryBody {
        content: body.content,
        kind: body.kind,
        scope: Scope::Workspace.as_str().to_string(),
        tags: body.tags,
        importance: body.importance,
    };
    save_into(&state, &Workspace::shared(), &id, body).await
}

#[derive(Deserialize)]
struct SearchParams {
    ws: Option<String>,
    q: Option<String>,
    scope: Option<String>,
    limit: Option<usize>,
    all: Option<bool>,
    /// true: only surface linked neighbors within the recall's scope. Default false.
    links_scope: Option<bool>,
}

async fn search<B: Backend>(
    State(state): State<AppState<B>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let q = params
        .q
        .ok_or_else(|| ApiError::BadRequest("missing required query parameter: q".into()))?;
    let args = SearchArgs {
        ws: params.ws,
        q,
        scope: params.scope,
        limit: params.limit,
        all: params.all.unwrap_or(false),
        links_scope: params.links_scope.unwrap_or(false),
    };
    Ok(Json(ops::search(&state.backend, args).await?))
}

#[derive(Deserialize)]
struct ContextParams {
    ws: Option<String>,
    project: Option<String>,
}

async fn context<B: Backend>(
    State(state): State<AppState<B>>,
    Query(params): Query<ContextParams>,
) -> Result<Json<ContextResponse>, ApiError> {
    let ws = require_ws(&WsQuery { ws: params.ws })?;
    Ok(Json(
        ops::context(&state.backend, &ws, params.project.as_deref()).await?,
    ))
}

async fn patch_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    Json(body): Json<PatchMemoryBody>,
) -> Result<Json<MemoryDto>, ApiError> {
    let ws = require_ws(&query)?;
    Ok(Json(ops::edit(&state.backend, &ws, &id, body).await?))
}

async fn patch_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Json(body): Json<PatchMemoryBody>,
) -> Result<Json<MemoryDto>, ApiError> {
    Ok(Json(
        ops::edit(&state.backend, &Workspace::shared(), &id, body).await?,
    ))
}

async fn delete_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Result<StatusCode, ApiError> {
    let ws = require_ws(&query)?;
    ops::forget(&state.backend, &ws, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    ops::forget(&state.backend, &Workspace::shared(), &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn move_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Json(body): Json<MoveBody>,
) -> Result<Json<MoveResponse>, ApiError> {
    Ok(Json(ops::move_memory(&state.backend, &id, body).await?))
}

async fn get_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Result<Json<MemoryDto>, ApiError> {
    let ws = require_ws(&query)?;
    Ok(Json(ops::fetch(&state.backend, &ws, &id).await?))
}

#[derive(Deserialize)]
struct LinksQuery {
    ws: Option<String>,
    depth: Option<usize>,
    target: Option<String>,
}

async fn links<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<LinksQuery>,
) -> Result<Json<GraphDto>, ApiError> {
    let ws = require_ws(&WsQuery { ws: query.ws })?;
    Ok(Json(
        ops::links_graph(&state.backend, &ws, &id, query.depth).await?,
    ))
}

/// Body for creating or retyping a link: the other endpoint and the noun relation.
#[derive(Deserialize)]
struct LinkBody {
    target: String,
    relation: String,
}

async fn create_link<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    Json(body): Json<LinkBody>,
) -> Result<StatusCode, ApiError> {
    let ws = require_ws(&query)?;
    ops::create_link(&state.backend, &ws, &id, &body.target, &body.relation).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn retype_link<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    Json(body): Json<LinkBody>,
) -> Result<StatusCode, ApiError> {
    let ws = require_ws(&query)?;
    ops::retype_link(&state.backend, &ws, &id, &body.target, &body.relation).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_link<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<LinksQuery>,
) -> Result<StatusCode, ApiError> {
    let ws = require_ws(&WsQuery { ws: query.ws })?;
    let target = query
        .target
        .ok_or_else(|| ApiError::BadRequest("missing required query parameter: target".into()))?;
    ops::delete_link(&state.backend, &ws, &id, &target).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
) -> Result<Json<MemoryDto>, ApiError> {
    Ok(Json(
        ops::fetch(&state.backend, &Workspace::shared(), &id).await?,
    ))
}

async fn list_memories<B: Backend>(
    State(state): State<AppState<B>>,
    Query(query): Query<WsQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let ws = require_ws(&query)?;
    Ok(Json(ops::list(&state.backend, &ws).await?))
}

async fn list_preferences<B: Backend>(
    State(state): State<AppState<B>>,
) -> Result<Json<ListResponse>, ApiError> {
    Ok(Json(ops::list(&state.backend, &Workspace::shared()).await?))
}

async fn export<B: Backend>(
    State(state): State<AppState<B>>,
    Query(query): Query<WsQuery>,
) -> Result<Json<ExportDoc>, ApiError> {
    let ws = require_ws(&query)?;
    Ok(Json(ops::export(&state.backend, &ws).await?))
}

async fn export_preferences<B: Backend>(
    State(state): State<AppState<B>>,
) -> Result<Json<ExportDoc>, ApiError> {
    Ok(Json(
        ops::export(&state.backend, &Workspace::shared()).await?,
    ))
}

#[derive(Deserialize)]
struct ImportParams {
    ws: Option<String>,
    mode: Option<String>,
}

async fn import<B: Backend>(
    State(state): State<AppState<B>>,
    Query(params): Query<ImportParams>,
    Json(doc): Json<ExportDoc>,
) -> Result<Json<ImportReport>, ApiError> {
    let ws = require_ws(&WsQuery { ws: params.ws })?;
    Ok(Json(
        ops::import(&state.backend, &ws, params.mode.as_deref(), doc).await?,
    ))
}

async fn import_preferences<B: Backend>(
    State(state): State<AppState<B>>,
    Query(params): Query<ImportParams>,
    Json(doc): Json<ExportDoc>,
) -> Result<Json<ImportReport>, ApiError> {
    Ok(Json(
        ops::import(
            &state.backend,
            &Workspace::shared(),
            params.mode.as_deref(),
            doc,
        )
        .await?,
    ))
}
