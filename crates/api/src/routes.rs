use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use domain::{
    EditRequest, Importance, Memory, MemoryId, MemoryKind, RECALL_LIMIT_CAP, RecallRequest,
    SaveOutcome, SaveRequest, Scope, Workspace,
};
use serde::Deserialize;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, MakeSpan, TraceLayer};
use tracing::Level;

use crate::backend::Backend;
use crate::dto::{
    ContextResponse, EXPORT_VERSION, ExportDoc, HealthResponse, HitDto, HitGroupDto, ImportReport,
    ListResponse, MemoryDto, MoveBody, MoveResponse, Origin, PatchMemoryBody, PutMemoryBody,
    PutPreferenceBody, SearchResponse, WorkspaceDto, WorkspacesResponse,
};
use crate::error::ApiError;
use crate::validate::{normalize_timestamp, validate_content, validate_query, validate_tags};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(600);
const BODY_LIMIT: usize = 32 * 1024 * 1024;
const DEFAULT_SEARCH_LIMIT: usize = 10;
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

fn parse_ws(name: &str) -> Result<Workspace, ApiError> {
    if name == "shared" {
        return Err(ApiError::BadRequest(
            "\"shared\" is not a workspace; memories that apply everywhere live under /preferences"
                .into(),
        ));
    }
    Workspace::new(name).map_err(ApiError::from)
}

fn workspace_of(origin: &Origin) -> Result<Workspace, ApiError> {
    match origin {
        Origin::Preference => Ok(Workspace::shared()),
        Origin::Workspace(name) => parse_ws(name),
    }
}

fn require_ws(query: &WsQuery) -> Result<Workspace, ApiError> {
    match query.ws.as_deref() {
        Some(name) => parse_ws(name),
        None => Err(ApiError::BadRequest(
            "missing required query parameter: ws".into(),
        )),
    }
}

fn parse_project(value: Option<&str>) -> Result<Option<String>, ApiError> {
    match value {
        None => Ok(None),
        Some(raw) => match Scope::parse(raw)? {
            Scope::Workspace => Ok(None),
            Scope::Project(slug) => Ok(Some(slug)),
        },
    }
}

async fn put_workspace<B: Backend>(
    State(state): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<WorkspaceDto>), ApiError> {
    let ws = Workspace::new(&name)?;
    let created = state.backend.create_workspace(&ws).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(WorkspaceDto {
            workspace: ws.to_string(),
        }),
    ))
}

async fn get_workspaces<B: Backend>(
    State(state): State<AppState<B>>,
) -> Result<Json<WorkspacesResponse>, ApiError> {
    let mut workspaces: Vec<String> = state
        .backend
        .workspaces()
        .await?
        .iter()
        .filter(|ws| !ws.is_shared())
        .map(Workspace::to_string)
        .collect();
    workspaces.sort();
    Ok(Json(WorkspacesResponse { workspaces }))
}

async fn save_into<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
    id: &str,
    body: PutMemoryBody,
) -> Result<(StatusCode, Json<MemoryDto>), ApiError> {
    let id = MemoryId::parse(id)?;
    validate_content(&state.backend, &body.content)?;
    validate_tags(&body.tags)?;
    let request = SaveRequest {
        id,
        content: body.content,
        kind: MemoryKind::parse(&body.kind)?,
        scope: Scope::parse(&body.scope)?,
        tags: body.tags,
        importance: body
            .importance
            .as_deref()
            .map(Importance::parse)
            .transpose()?,
    };
    match state.backend.save(ws, request).await? {
        SaveOutcome::Created(memory) => Ok((StatusCode::CREATED, Json(MemoryDto::from(&memory)))),
        SaveOutcome::Unchanged(memory) => Ok((StatusCode::OK, Json(MemoryDto::from(&memory)))),
    }
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
}

async fn search<B: Backend>(
    State(state): State<AppState<B>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let ws = params.ws.as_deref().map(parse_ws).transpose()?;
    let query = params
        .q
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("missing required query parameter: q".into()))?;
    validate_query(&state.backend, query)?;
    let limit = params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    if !(1..=RECALL_LIMIT_CAP).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {RECALL_LIMIT_CAP}"
        )));
    }
    let request = RecallRequest {
        query: query.to_string(),
        project: parse_project(params.scope.as_deref())?,
        limit,
    };
    if params.all.unwrap_or(false) {
        let groups = state.backend.recall_all(&request).await?;
        Ok(Json(SearchResponse::Grouped {
            groups: groups.iter().map(HitGroupDto::from).collect(),
        }))
    } else {
        let ws =
            ws.ok_or_else(|| ApiError::BadRequest("missing required query parameter: ws".into()))?;
        let hits = state.backend.recall(&ws, &request).await?;
        Ok(Json(SearchResponse::Flat {
            hits: hits.iter().map(HitDto::from).collect(),
        }))
    }
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
    let project = parse_project(params.project.as_deref())?;
    let digest = state.backend.context(&ws, project.as_deref()).await?;
    Ok(Json(ContextResponse::from(&digest)))
}

async fn edit_in<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
    id: &str,
    body: PatchMemoryBody,
) -> Result<Json<MemoryDto>, ApiError> {
    let id = MemoryId::parse(id)?;
    if let Some(content) = &body.content {
        validate_content(&state.backend, content)?;
    }
    if let Some(tags) = &body.tags {
        validate_tags(tags)?;
    }
    let request = EditRequest {
        content: body.content,
        tags: body.tags,
        pinned: body.pinned,
        importance: body
            .importance
            .as_deref()
            .map(Importance::parse)
            .transpose()?,
    };
    let memory = state.backend.edit(ws, &id, request).await?;
    Ok(Json(MemoryDto::from(&memory)))
}

async fn patch_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    Json(body): Json<PatchMemoryBody>,
) -> Result<Json<MemoryDto>, ApiError> {
    let ws = require_ws(&query)?;
    edit_in(&state, &ws, &id, body).await
}

async fn patch_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Json(body): Json<PatchMemoryBody>,
) -> Result<Json<MemoryDto>, ApiError> {
    edit_in(&state, &Workspace::shared(), &id, body).await
}

async fn forget_in<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
    id: &str,
) -> Result<StatusCode, ApiError> {
    let id = MemoryId::parse(id)?;
    state.backend.forget(ws, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Result<StatusCode, ApiError> {
    let ws = require_ws(&query)?;
    forget_in(&state, &ws, &id).await
}

async fn delete_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    forget_in(&state, &Workspace::shared(), &id).await
}

async fn move_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Json(body): Json<MoveBody>,
) -> Result<Json<MoveResponse>, ApiError> {
    let id = MemoryId::parse(&id)?;
    let from = workspace_of(&body.from)?;
    let to = workspace_of(&body.to)?;
    let outcome = state.backend.move_memory(&from, &to, &id).await?;
    Ok(Json(MoveResponse {
        moved: outcome.moved,
        from: body.from,
        to: body.to,
        from_scope: outcome.from_scope.as_str().to_string(),
        memory: MemoryDto::from(&outcome.memory),
    }))
}

async fn fetch_in<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
    id: &str,
) -> Result<Json<MemoryDto>, ApiError> {
    let id = MemoryId::parse(id)?;
    let memory = state
        .backend
        .get(ws, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("memory {id} not found")))?;
    Ok(Json(MemoryDto::from(&memory)))
}

async fn get_memory<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Result<Json<MemoryDto>, ApiError> {
    let ws = require_ws(&query)?;
    fetch_in(&state, &ws, &id).await
}

async fn get_preference<B: Backend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
) -> Result<Json<MemoryDto>, ApiError> {
    fetch_in(&state, &Workspace::shared(), &id).await
}

async fn list_in<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
) -> Result<Json<ListResponse>, ApiError> {
    let memories = state.backend.list(ws).await?;
    Ok(Json(ListResponse {
        memories: memories.iter().map(MemoryDto::from).collect(),
    }))
}

async fn list_memories<B: Backend>(
    State(state): State<AppState<B>>,
    Query(query): Query<WsQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let ws = require_ws(&query)?;
    list_in(&state, &ws).await
}

async fn list_preferences<B: Backend>(
    State(state): State<AppState<B>>,
) -> Result<Json<ListResponse>, ApiError> {
    list_in(&state, &Workspace::shared()).await
}

async fn export_in<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
) -> Result<Json<ExportDoc>, ApiError> {
    let memories = state.backend.list(ws).await?;
    Ok(Json(ExportDoc {
        version: EXPORT_VERSION,
        origin: Origin::of(ws),
        memories: memories.iter().map(MemoryDto::from).collect(),
    }))
}

async fn export<B: Backend>(
    State(state): State<AppState<B>>,
    Query(query): Query<WsQuery>,
) -> Result<Json<ExportDoc>, ApiError> {
    let ws = require_ws(&query)?;
    export_in(&state, &ws).await
}

async fn export_preferences<B: Backend>(
    State(state): State<AppState<B>>,
) -> Result<Json<ExportDoc>, ApiError> {
    export_in(&state, &Workspace::shared()).await
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
    import_into(&state, &ws, params.mode.as_deref(), doc).await
}

async fn import_preferences<B: Backend>(
    State(state): State<AppState<B>>,
    Query(params): Query<ImportParams>,
    Json(doc): Json<ExportDoc>,
) -> Result<Json<ImportReport>, ApiError> {
    import_into(&state, &Workspace::shared(), params.mode.as_deref(), doc).await
}

async fn import_into<B: Backend>(
    state: &AppState<B>,
    ws: &Workspace,
    mode: Option<&str>,
    doc: ExportDoc,
) -> Result<Json<ImportReport>, ApiError> {
    let merge = match mode {
        None | Some("fail-if-nonempty") => false,
        Some("merge") => true,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "invalid mode {other:?}: expected fail-if-nonempty or merge"
            )));
        }
    };
    if doc.version != EXPORT_VERSION {
        return Err(ApiError::BadRequest(format!(
            "unsupported export version {}, this server reads version {EXPORT_VERSION}",
            doc.version
        )));
    }
    let into_preferences = ws.is_shared();
    if into_preferences != matches!(doc.origin, Origin::Preference) {
        return Err(ApiError::BadRequest(if into_preferences {
            "this dump came from a workspace; import it with /import?ws=<workspace>".into()
        } else {
            "this dump holds preferences; import it with /preferences/import".into()
        }));
    }
    let mut memories: Vec<Memory> = Vec::with_capacity(doc.memories.len());
    for dto in &doc.memories {
        let item = |err: ApiError| match err {
            ApiError::BadRequest(msg) => ApiError::BadRequest(format!("memory {}: {msg}", dto.id)),
            other => other,
        };
        validate_content(&state.backend, &dto.content).map_err(item)?;
        validate_tags(&dto.tags).map_err(item)?;
        let created_at = normalize_timestamp(&dto.created_at).map_err(item)?;
        let updated_at = normalize_timestamp(&dto.updated_at).map_err(item)?;
        let mut memory = dto.to_memory().map_err(|e| item(ApiError::from(e)))?;
        if into_preferences && memory.scope != Scope::Workspace {
            return Err(item(ApiError::BadRequest(format!(
                "preferences apply everywhere and cannot carry the project scope {:?}",
                memory.scope.as_str()
            ))));
        }
        memory.created_at = created_at;
        memory.updated_at = updated_at;
        memories.push(memory);
    }
    if !merge {
        let incoming: std::collections::HashSet<&str> =
            memories.iter().map(|m| m.id.as_str()).collect();
        let stray = state
            .backend
            .list(ws)
            .await?
            .into_iter()
            .find(|existing| !incoming.contains(existing.id.as_str()));
        if let Some(stray) = stray {
            return Err(ApiError::Conflict(format!(
                "{} already holds memory {} which this dump does not contain; \
                 use mode=merge to import into it",
                Origin::of(ws).label(),
                stray.id
            )));
        }
    }
    let report = state.backend.restore(ws, memories).await?;
    Ok(Json(ImportReport {
        imported: report.imported,
        unchanged: report.unchanged,
    }))
}
