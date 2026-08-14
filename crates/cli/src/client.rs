use api::{
    ContextResponse, ExportDoc, GraphDto, ImportReport, LinkCandidateDto, MemoryDto, MoveBody,
    MoveResponse, Origin, PatchMemoryBody, PutMemoryBody, PutPreferenceBody, SearchResponse,
};
use daemon_client::DaemonClient;
use domain::Scope;

use crate::outbox::{SaveTarget, SendFailure};

/// The daemon, with the CLI's two-store vocabulary mapped onto the wire's `Origin`: a named
/// workspace, or the preferences every workspace can reach.
pub struct Client(DaemonClient);

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn preference_body(body: PutPreferenceBody) -> PutMemoryBody {
    PutMemoryBody {
        content: body.content,
        title: body.title,
        kind: body.kind,
        scope: Scope::Workspace.as_str().to_string(),
        tags: body.tags,
        importance: body.importance,
    }
}

impl Client {
    pub fn new(daemon: DaemonClient) -> Self {
        Self(daemon)
    }

    /// The outbox's send hook: it classifies its own failures, so the queue's
    /// keep-or-dead-letter call lives with the transport that produced the error.
    pub fn send_save(
        &self,
        id: &str,
        target: &SaveTarget,
    ) -> Result<Vec<LinkCandidateDto>, SendFailure> {
        match target {
            SaveTarget::Memory { workspace, body } => {
                self.0
                    .save(Origin::Workspace(workspace.clone()), id, body.clone())
            }
            SaveTarget::Preference { body } => {
                self.0
                    .save(Origin::Preference, id, preference_body(body.clone()))
            }
        }
        .map(|saved| saved.candidates)
        .map_err(|e| SendFailure {
            retryable: e.is_retryable(),
            invalid: e.is_invalid_request(),
            message: e.to_string(),
        })
    }

    pub fn create_workspace(&self, name: &str) -> Result<String, String> {
        Ok(self.0.create_workspace(name).map_err(err)?.workspace)
    }

    pub fn workspaces(&self) -> Result<Vec<String>, String> {
        self.0.workspaces().map_err(err)
    }

    pub fn save(
        &self,
        workspace: &str,
        id: &str,
        body: &PutMemoryBody,
    ) -> Result<(MemoryDto, Vec<LinkCandidateDto>), String> {
        let saved = self
            .0
            .save(Origin::Workspace(workspace.to_string()), id, body.clone())
            .map_err(err)?;
        Ok((saved.memory, saved.candidates))
    }

    pub fn save_preference(
        &self,
        id: &str,
        body: &PutPreferenceBody,
    ) -> Result<(MemoryDto, Vec<LinkCandidateDto>), String> {
        let saved = self
            .0
            .save(Origin::Preference, id, preference_body(body.clone()))
            .map_err(err)?;
        Ok((saved.memory, saved.candidates))
    }

    pub fn edit(
        &self,
        workspace: &str,
        id: &str,
        body: &PatchMemoryBody,
    ) -> Result<MemoryDto, String> {
        self.0
            .edit(Origin::Workspace(workspace.to_string()), id, body.clone())
            .map_err(err)
    }

    pub fn edit_preference(&self, id: &str, body: &PatchMemoryBody) -> Result<MemoryDto, String> {
        self.0
            .edit(Origin::Preference, id, body.clone())
            .map_err(err)
    }

    pub fn forget(&self, workspace: &str, id: &str) -> Result<(), String> {
        self.0
            .forget(Origin::Workspace(workspace.to_string()), id)
            .map_err(err)
    }

    pub fn forget_preference(&self, id: &str) -> Result<(), String> {
        self.0.forget(Origin::Preference, id).map_err(err)
    }

    pub fn get(&self, workspace: &str, id: &str) -> Result<MemoryDto, String> {
        self.0
            .get(Origin::Workspace(workspace.to_string()), id)
            .map_err(err)
    }

    pub fn get_preference(&self, id: &str) -> Result<MemoryDto, String> {
        self.0.get(Origin::Preference, id).map_err(err)
    }

    pub fn list(&self, workspace: &str) -> Result<Vec<MemoryDto>, String> {
        self.0
            .list(Origin::Workspace(workspace.to_string()))
            .map_err(err)
    }

    pub fn list_preferences(&self) -> Result<Vec<MemoryDto>, String> {
        self.0.list(Origin::Preference).map_err(err)
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
        self.0
            .search(
                workspace,
                query,
                scope,
                limit,
                all_workspaces,
                links_in_scope,
            )
            .map_err(err)
    }

    pub fn context(
        &self,
        workspace: &str,
        project: Option<&str>,
    ) -> Result<ContextResponse, String> {
        self.0.context(workspace, project).map_err(err)
    }

    pub fn move_memory(&self, id: &str, body: &MoveBody) -> Result<MoveResponse, String> {
        self.0.move_memory(id, body.clone()).map_err(err)
    }

    pub fn links(&self, workspace: &str, id: &str, depth: usize) -> Result<GraphDto, String> {
        self.0.links(workspace, id, depth).map_err(err)
    }

    pub fn link(
        &self,
        workspace: &str,
        source: &str,
        target: &str,
        relation: &str,
    ) -> Result<(), String> {
        self.0
            .link(workspace, source, target, relation)
            .map_err(err)
    }

    pub fn retype_link(
        &self,
        workspace: &str,
        a: &str,
        b: &str,
        relation: &str,
    ) -> Result<(), String> {
        self.0.retype_link(workspace, a, b, relation).map_err(err)
    }

    pub fn unlink(&self, workspace: &str, a: &str, b: &str) -> Result<(), String> {
        self.0.unlink(workspace, a, b).map_err(err)
    }

    pub fn export(&self, workspace: &str) -> Result<ExportDoc, String> {
        self.0
            .export(Origin::Workspace(workspace.to_string()))
            .map_err(err)
    }

    pub fn export_preferences(&self) -> Result<ExportDoc, String> {
        self.0.export(Origin::Preference).map_err(err)
    }

    pub fn import(
        &self,
        workspace: &str,
        merge: bool,
        doc: &ExportDoc,
    ) -> Result<ImportReport, String> {
        self.0
            .import(Origin::Workspace(workspace.to_string()), merge, doc.clone())
            .map_err(err)
    }

    pub fn import_preferences(&self, merge: bool, doc: &ExportDoc) -> Result<ImportReport, String> {
        self.0
            .import(Origin::Preference, merge, doc.clone())
            .map_err(err)
    }
}
