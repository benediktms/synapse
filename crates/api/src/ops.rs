//! Transport-neutral operations behind both the HTTP router and the daemon's JSON-RPC
//! dispatch. Handlers stay thin: parameter extraction and status codes live at the
//! transport; validation and DTO mapping live here, once.

use domain::{
    EditRequest, Importance, Link, MAX_GRAPH_DEPTH, Memory, MemoryId, MemoryKind, RECALL_LIMIT_CAP,
    RecallRequest, Relation, SaveOutcome, SaveRequest, Scope, Workspace,
};

use crate::backend::Backend;
use crate::dto::{
    ContextResponse, EXPORT_VERSION, ExportDoc, GraphDto, HitDto, HitGroupDto, ImportReport,
    LinkCandidateDto, LinkDto, ListResponse, MemoryDto, MoveBody, MoveResponse, Origin,
    PatchMemoryBody, PutMemoryBody, SearchResponse, WorkspaceDto, WorkspacesResponse,
};
use crate::error::ApiError;
use crate::validate::{
    normalize_timestamp, validate_content, validate_query, validate_tags, validate_title,
};

pub const DEFAULT_SEARCH_LIMIT: usize = 10;
pub const DEFAULT_GRAPH_DEPTH: usize = 2;

pub fn parse_ws(name: &str) -> Result<Workspace, ApiError> {
    if name == "shared" {
        return Err(ApiError::BadRequest(
            "\"shared\" is not a workspace; memories that apply everywhere live under /preferences"
                .into(),
        ));
    }
    Workspace::new(name).map_err(ApiError::from)
}

pub fn workspace_of(origin: &Origin) -> Result<Workspace, ApiError> {
    match origin {
        Origin::Preference => Ok(Workspace::shared()),
        Origin::Workspace(name) => parse_ws(name),
    }
}

pub fn parse_project(value: Option<&str>) -> Result<Option<String>, ApiError> {
    match value {
        None => Ok(None),
        Some(raw) => match Scope::parse(raw)? {
            Scope::Workspace => Ok(None),
            Scope::Project(slug) => Ok(Some(slug)),
        },
    }
}

#[derive(serde::Serialize)]
pub struct Saved {
    pub created: bool,
    pub memory: MemoryDto,
    /// Memories the store already held that closely resemble this one. Advisory only — nothing
    /// is linked, because only the writer can name the relation.
    #[serde(default)]
    pub candidates: Vec<LinkCandidateDto>,
}

pub async fn create_workspace<B: Backend>(
    backend: &B,
    name: &str,
) -> Result<(bool, WorkspaceDto), ApiError> {
    let ws = Workspace::new(name)?;
    let created = backend.create_workspace(&ws).await?;
    Ok((
        created,
        WorkspaceDto {
            workspace: ws.to_string(),
        },
    ))
}

pub async fn workspaces<B: Backend>(backend: &B) -> Result<WorkspacesResponse, ApiError> {
    let mut workspaces: Vec<String> = backend
        .workspaces()
        .await?
        .iter()
        .filter(|ws| !ws.is_shared())
        .map(Workspace::to_string)
        .collect();
    workspaces.sort();
    Ok(WorkspacesResponse { workspaces })
}

pub async fn save<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
    body: PutMemoryBody,
) -> Result<Saved, ApiError> {
    let id = MemoryId::parse(id)?;
    validate_content(backend, &body.content)?;
    validate_tags(&body.tags)?;
    if let Some(title) = &body.title {
        validate_title(title, false)?;
    }
    let request = SaveRequest {
        id,
        content: body.content,
        title: body.title,
        kind: MemoryKind::parse(&body.kind)?,
        scope: Scope::parse(&body.scope)?,
        tags: body.tags,
        importance: body
            .importance
            .as_deref()
            .map(Importance::parse)
            .transpose()?,
    };
    match backend.save(ws, request).await? {
        SaveOutcome::Created(memory, candidates) => Ok(Saved {
            created: true,
            memory: MemoryDto::from(&memory),
            candidates: candidates.iter().map(LinkCandidateDto::from).collect(),
        }),
        SaveOutcome::Unchanged(memory) => Ok(Saved {
            created: false,
            memory: MemoryDto::from(&memory),
            candidates: Vec::new(),
        }),
    }
}

pub async fn edit<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
    body: PatchMemoryBody,
) -> Result<MemoryDto, ApiError> {
    let id = MemoryId::parse(id)?;
    if let Some(content) = &body.content {
        validate_content(backend, content)?;
    }
    if let Some(tags) = &body.tags {
        validate_tags(tags)?;
    }
    if let Some(title) = &body.title {
        validate_title(title, false)?;
    }
    let request = EditRequest {
        content: body.content,
        title: body.title,
        tags: body.tags,
        pinned: body.pinned,
        importance: body
            .importance
            .as_deref()
            .map(Importance::parse)
            .transpose()?,
    };
    let memory = backend.edit(ws, &id, request).await?;
    Ok(MemoryDto::from(&memory))
}

pub async fn forget<B: Backend>(backend: &B, ws: &Workspace, id: &str) -> Result<(), ApiError> {
    let id = MemoryId::parse(id)?;
    backend.forget(ws, &id).await?;
    Ok(())
}

pub async fn move_memory<B: Backend>(
    backend: &B,
    id: &str,
    body: MoveBody,
) -> Result<MoveResponse, ApiError> {
    let id = MemoryId::parse(id)?;
    let from = workspace_of(&body.from)?;
    let to = workspace_of(&body.to)?;
    let outcome = backend.move_memory(&from, &to, &id).await?;
    Ok(MoveResponse {
        moved: outcome.moved,
        from: body.from,
        to: body.to,
        from_scope: outcome.from_scope.as_str().to_string(),
        links_dropped: outcome.links_dropped,
        memory: MemoryDto::from(&outcome.memory),
    })
}

pub async fn fetch<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
) -> Result<MemoryDto, ApiError> {
    let id = MemoryId::parse(id)?;
    let memory = backend
        .get(ws, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("memory {id} not found")))?;
    Ok(MemoryDto::from(&memory))
}

pub async fn list<B: Backend>(backend: &B, ws: &Workspace) -> Result<ListResponse, ApiError> {
    let memories = backend.list(ws).await?;
    Ok(ListResponse {
        memories: memories.iter().map(MemoryDto::from).collect(),
    })
}

pub struct SearchArgs {
    pub ws: Option<String>,
    pub q: String,
    pub scope: Option<String>,
    pub limit: Option<usize>,
    pub all: bool,
    pub links_scope: bool,
}

pub async fn search<B: Backend>(backend: &B, args: SearchArgs) -> Result<SearchResponse, ApiError> {
    let ws = args.ws.as_deref().map(parse_ws).transpose()?;
    validate_query(backend, &args.q)?;
    let limit = args.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    if !(1..=RECALL_LIMIT_CAP).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {RECALL_LIMIT_CAP}"
        )));
    }
    let request = RecallRequest {
        query: args.q,
        project: parse_project(args.scope.as_deref())?,
        limit,
        links_in_scope: args.links_scope,
    };
    if args.all {
        let groups = backend.recall_all(&request).await?;
        Ok(SearchResponse::Grouped {
            groups: groups.iter().map(HitGroupDto::from).collect(),
        })
    } else {
        let ws =
            ws.ok_or_else(|| ApiError::BadRequest("missing required query parameter: ws".into()))?;
        let hits = backend.recall(&ws, &request).await?;
        Ok(SearchResponse::Flat {
            hits: hits.iter().map(HitDto::from).collect(),
        })
    }
}

pub async fn context<B: Backend>(
    backend: &B,
    ws: &Workspace,
    project: Option<&str>,
) -> Result<ContextResponse, ApiError> {
    let project = parse_project(project)?;
    let digest = backend.context(ws, project.as_deref()).await?;
    Ok(ContextResponse::from(&digest))
}

pub async fn links_graph<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
    depth: Option<usize>,
) -> Result<GraphDto, ApiError> {
    let id = MemoryId::parse(id)?;
    let depth = depth.unwrap_or(DEFAULT_GRAPH_DEPTH);
    if depth > MAX_GRAPH_DEPTH {
        return Err(ApiError::BadRequest(format!(
            "depth must be at most {MAX_GRAPH_DEPTH}"
        )));
    }
    let sub = backend.links(ws, &id, depth).await?;
    Ok(GraphDto::from(&sub))
}

pub async fn create_link<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
    target: &str,
    relation: &str,
) -> Result<(), ApiError> {
    let source = MemoryId::parse(id)?;
    let target = MemoryId::parse(target)?;
    let relation = Relation::parse(relation)?;
    backend.link(ws, &source, &target, relation).await?;
    Ok(())
}

pub async fn retype_link<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
    target: &str,
    relation: &str,
) -> Result<(), ApiError> {
    let a = MemoryId::parse(id)?;
    let b = MemoryId::parse(target)?;
    let relation = Relation::parse(relation)?;
    backend.retype_link(ws, &a, &b, relation).await?;
    Ok(())
}

pub async fn delete_link<B: Backend>(
    backend: &B,
    ws: &Workspace,
    id: &str,
    target: &str,
) -> Result<usize, ApiError> {
    let a = MemoryId::parse(id)?;
    let b = MemoryId::parse(target)?;
    Ok(backend.unlink(ws, &a, &b).await?)
}

pub async fn export<B: Backend>(backend: &B, ws: &Workspace) -> Result<ExportDoc, ApiError> {
    let memories = backend.list(ws).await?;
    let links = backend.links_all(ws).await?;
    Ok(ExportDoc {
        version: EXPORT_VERSION,
        origin: Origin::of(ws),
        memories: memories.iter().map(MemoryDto::from).collect(),
        links: links.iter().map(LinkDto::from).collect(),
    })
}

fn link_err(link: &LinkDto, err: domain::Error) -> ApiError {
    match ApiError::from(err) {
        ApiError::BadRequest(msg) => {
            ApiError::BadRequest(format!("link {} → {}: {msg}", link.source, link.target))
        }
        other => other,
    }
}

pub async fn import<B: Backend>(
    backend: &B,
    ws: &Workspace,
    mode: Option<&str>,
    doc: ExportDoc,
) -> Result<ImportReport, ApiError> {
    let merge = match mode {
        None | Some("fail-if-nonempty") => false,
        Some("merge") => true,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "invalid mode {other:?}: expected fail-if-nonempty or merge"
            )));
        }
    };
    // v1 (linkless) dumps stay importable as backups made before links existed; only v2 is emitted.
    if doc.version != EXPORT_VERSION && doc.version != 1 {
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
        validate_content(backend, &dto.content).map_err(item)?;
        validate_tags(&dto.tags).map_err(item)?;
        validate_title(&dto.title, true).map_err(item)?;
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
    if into_preferences && !doc.links.is_empty() {
        return Err(ApiError::BadRequest(
            "preferences carry no links; every link command acts on workspace memories".into(),
        ));
    }
    let known: std::collections::HashSet<&str> = memories.iter().map(|m| m.id.as_str()).collect();
    let mut parsed_links: Vec<Link> = Vec::with_capacity(doc.links.len());
    for link in &doc.links {
        let source = MemoryId::parse(&link.source).map_err(|e| link_err(link, e))?;
        let target = MemoryId::parse(&link.target).map_err(|e| link_err(link, e))?;
        if !known.contains(link.source.as_str()) || !known.contains(link.target.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "link {} → {} references a memory this dump does not contain",
                link.source, link.target
            )));
        }
        let relation = Relation::parse(&link.relation).map_err(|e| link_err(link, e))?;
        parsed_links.push(Link {
            source,
            target,
            relation,
        });
    }
    if !merge {
        let incoming: std::collections::HashSet<&str> =
            memories.iter().map(|m| m.id.as_str()).collect();
        let stray = backend
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
        let dump: std::collections::HashSet<(String, String, Relation)> = parsed_links
            .iter()
            .cloned()
            .map(|link| {
                let link = link.canonical();
                (
                    link.source.to_string(),
                    link.target.to_string(),
                    link.relation,
                )
            })
            .collect();
        let extra = backend.links_all(ws).await?.into_iter().find(|edge| {
            !dump.contains(&(
                edge.source.to_string(),
                edge.target.to_string(),
                edge.relation,
            ))
        });
        if let Some(extra) = extra {
            return Err(ApiError::Conflict(format!(
                "{} already holds a {} link {} → {} which this dump does not contain; \
                 use mode=merge to import into it",
                Origin::of(ws).label(),
                extra.relation.as_str(),
                extra.source,
                extra.target
            )));
        }
    }
    let report = backend.restore(ws, memories, parsed_links).await?;
    Ok(ImportReport {
        imported: report.imported,
        unchanged: report.unchanged,
    })
}
