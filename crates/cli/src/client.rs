use api::{
    ContextResponse, ExportDoc, GraphDto, ImportReport, MemoryDto, MoveBody, MoveResponse, Origin,
    PatchMemoryBody, PutMemoryBody, PutPreferenceBody, SearchResponse,
};
use api_client::SynapseApiClient;
use daemon_client::DaemonClient;
use domain::Scope;

/// The backend a command talks to. Both transports expose the HTTP client's method
/// surface, so commands stay transport-blind; only the outbox paths branch.
pub enum Client {
    Http(SynapseApiClient),
    Daemon(DaemonClient),
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn preference_body(body: PutPreferenceBody) -> PutMemoryBody {
    PutMemoryBody {
        content: body.content,
        kind: body.kind,
        scope: Scope::Workspace.as_str().to_string(),
        tags: body.tags,
        importance: body.importance,
    }
}

impl Client {
    pub fn create_workspace(&self, name: &str) -> Result<String, String> {
        match self {
            Self::Http(c) => Ok(c.create_workspace(name).map_err(err)?.workspace),
            Self::Daemon(d) => Ok(d.create_workspace(name).map_err(err)?.workspace),
        }
    }

    pub fn workspaces(&self) -> Result<Vec<String>, String> {
        match self {
            Self::Http(c) => c.workspaces().map_err(err),
            Self::Daemon(d) => d.workspaces().map_err(err),
        }
    }

    pub fn save(
        &self,
        workspace: &str,
        id: &str,
        body: &PutMemoryBody,
    ) -> Result<MemoryDto, String> {
        match self {
            Self::Http(c) => c.save(workspace, id, body).map_err(err),
            Self::Daemon(d) => Ok(d
                .save(Origin::Workspace(workspace.to_string()), id, body.clone())
                .map_err(err)?
                .memory),
        }
    }

    pub fn save_preference(&self, id: &str, body: &PutPreferenceBody) -> Result<MemoryDto, String> {
        match self {
            Self::Http(c) => c.save_preference(id, body).map_err(err),
            Self::Daemon(d) => Ok(d
                .save(Origin::Preference, id, preference_body(body.clone()))
                .map_err(err)?
                .memory),
        }
    }

    pub fn edit(
        &self,
        workspace: &str,
        id: &str,
        body: &PatchMemoryBody,
    ) -> Result<MemoryDto, String> {
        match self {
            Self::Http(c) => c.edit(workspace, id, body).map_err(err),
            Self::Daemon(d) => d
                .edit(Origin::Workspace(workspace.to_string()), id, body.clone())
                .map_err(err),
        }
    }

    pub fn edit_preference(&self, id: &str, body: &PatchMemoryBody) -> Result<MemoryDto, String> {
        match self {
            Self::Http(c) => c.edit_preference(id, body).map_err(err),
            Self::Daemon(d) => d.edit(Origin::Preference, id, body.clone()).map_err(err),
        }
    }

    pub fn forget(&self, workspace: &str, id: &str) -> Result<(), String> {
        match self {
            Self::Http(c) => c.forget(workspace, id).map_err(err),
            Self::Daemon(d) => d
                .forget(Origin::Workspace(workspace.to_string()), id)
                .map_err(err),
        }
    }

    pub fn forget_preference(&self, id: &str) -> Result<(), String> {
        match self {
            Self::Http(c) => c.forget_preference(id).map_err(err),
            Self::Daemon(d) => d.forget(Origin::Preference, id).map_err(err),
        }
    }

    pub fn get(&self, workspace: &str, id: &str) -> Result<MemoryDto, String> {
        match self {
            Self::Http(c) => c.get(workspace, id).map_err(err),
            Self::Daemon(d) => d
                .get(Origin::Workspace(workspace.to_string()), id)
                .map_err(err),
        }
    }

    pub fn get_preference(&self, id: &str) -> Result<MemoryDto, String> {
        match self {
            Self::Http(c) => c.get_preference(id).map_err(err),
            Self::Daemon(d) => d.get(Origin::Preference, id).map_err(err),
        }
    }

    pub fn list(&self, workspace: &str) -> Result<Vec<MemoryDto>, String> {
        match self {
            Self::Http(c) => c.list(workspace).map_err(err),
            Self::Daemon(d) => d
                .list(Origin::Workspace(workspace.to_string()))
                .map_err(err),
        }
    }

    pub fn list_preferences(&self) -> Result<Vec<MemoryDto>, String> {
        match self {
            Self::Http(c) => c.list_preferences().map_err(err),
            Self::Daemon(d) => d.list(Origin::Preference).map_err(err),
        }
    }

    pub fn search(
        &self,
        workspace: &str,
        query: &str,
        scope: Option<&str>,
        limit: usize,
        all_workspaces: bool,
        links_in_scope: bool,
    ) -> Result<SearchResponse, String> {
        match self {
            Self::Http(c) => c
                .search(
                    workspace,
                    query,
                    scope,
                    limit,
                    all_workspaces,
                    links_in_scope,
                )
                .map_err(err),
            Self::Daemon(d) => d
                .search(
                    workspace,
                    query,
                    scope,
                    limit,
                    all_workspaces,
                    links_in_scope,
                )
                .map_err(err),
        }
    }

    pub fn context(
        &self,
        workspace: &str,
        project: Option<&str>,
    ) -> Result<ContextResponse, String> {
        match self {
            Self::Http(c) => c.context(workspace, project).map_err(err),
            Self::Daemon(d) => d.context(workspace, project).map_err(err),
        }
    }

    pub fn move_memory(&self, id: &str, body: &MoveBody) -> Result<MoveResponse, String> {
        match self {
            Self::Http(c) => c.move_memory(id, body).map_err(err),
            Self::Daemon(d) => d.move_memory(id, body.clone()).map_err(err),
        }
    }

    pub fn links(&self, workspace: &str, id: &str, depth: usize) -> Result<GraphDto, String> {
        match self {
            Self::Http(c) => c.links(workspace, id, depth).map_err(err),
            Self::Daemon(d) => d.links(workspace, id, depth).map_err(err),
        }
    }

    pub fn link(
        &self,
        workspace: &str,
        source: &str,
        target: &str,
        relation: &str,
    ) -> Result<(), String> {
        match self {
            Self::Http(c) => c.link(workspace, source, target, relation).map_err(err),
            Self::Daemon(d) => d.link(workspace, source, target, relation).map_err(err),
        }
    }

    pub fn retype_link(
        &self,
        workspace: &str,
        a: &str,
        b: &str,
        relation: &str,
    ) -> Result<(), String> {
        match self {
            Self::Http(c) => c.retype_link(workspace, a, b, relation).map_err(err),
            Self::Daemon(d) => d.retype_link(workspace, a, b, relation).map_err(err),
        }
    }

    pub fn unlink(&self, workspace: &str, a: &str, b: &str) -> Result<(), String> {
        match self {
            Self::Http(c) => c.unlink(workspace, a, b).map_err(err),
            Self::Daemon(d) => d.unlink(workspace, a, b).map_err(err),
        }
    }

    pub fn export(&self, workspace: &str) -> Result<ExportDoc, String> {
        match self {
            Self::Http(c) => c.export(workspace).map_err(err),
            Self::Daemon(d) => d
                .export(Origin::Workspace(workspace.to_string()))
                .map_err(err),
        }
    }

    pub fn export_preferences(&self) -> Result<ExportDoc, String> {
        match self {
            Self::Http(c) => c.export_preferences().map_err(err),
            Self::Daemon(d) => d.export(Origin::Preference).map_err(err),
        }
    }

    pub fn import(
        &self,
        workspace: &str,
        merge: bool,
        doc: &ExportDoc,
    ) -> Result<ImportReport, String> {
        match self {
            Self::Http(c) => c.import(workspace, merge, doc).map_err(err),
            Self::Daemon(d) => d
                .import(Origin::Workspace(workspace.to_string()), merge, doc.clone())
                .map_err(err),
        }
    }

    pub fn import_preferences(&self, merge: bool, doc: &ExportDoc) -> Result<ImportReport, String> {
        match self {
            Self::Http(c) => c.import_preferences(merge, doc).map_err(err),
            Self::Daemon(d) => d
                .import(Origin::Preference, merge, doc.clone())
                .map_err(err),
        }
    }
}
