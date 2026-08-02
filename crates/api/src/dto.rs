use domain::{
    ContextDigest, DigestEntry, Memory, MemoryId, MemoryKind, RecallHit, Scope, Timestamp,
    WorkspaceHits,
};
use serde::{Deserialize, Serialize};

pub const EXPORT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryDto {
    pub id: String,
    pub content: String,
    pub kind: String,
    pub scope: String,
    pub tags: Vec<String>,
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&Memory> for MemoryDto {
    fn from(memory: &Memory) -> Self {
        Self {
            id: memory.id.to_string(),
            content: memory.content.clone(),
            kind: memory.kind.as_str().to_string(),
            scope: memory.scope.as_str().to_string(),
            tags: memory.tags.clone(),
            pinned: memory.pinned,
            created_at: memory.created_at.to_string(),
            updated_at: memory.updated_at.to_string(),
        }
    }
}

impl MemoryDto {
    pub fn to_memory(&self) -> Result<Memory, domain::Error> {
        Ok(Memory {
            id: MemoryId::parse(&self.id)?,
            content: self.content.clone(),
            kind: MemoryKind::parse(&self.kind)?,
            scope: Scope::parse(&self.scope)?,
            tags: self.tags.clone(),
            pinned: self.pinned,
            created_at: Timestamp::new(self.created_at.clone()),
            updated_at: Timestamp::new(self.updated_at.clone()),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutMemoryBody {
    pub content: String,
    pub kind: String,
    pub scope: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PatchMemoryBody {
    pub content: Option<String>,
    pub tags: Option<Vec<String>>,
    pub pinned: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitDto {
    pub workspace: String,
    pub score: f64,
    #[serde(flatten)]
    pub memory: MemoryDto,
}

impl From<&RecallHit> for HitDto {
    fn from(hit: &RecallHit) -> Self {
        Self {
            workspace: hit.workspace.to_string(),
            score: hit.score,
            memory: MemoryDto::from(&hit.memory),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceHitsDto {
    pub workspace: String,
    pub hits: Vec<HitDto>,
}

impl From<&WorkspaceHits> for WorkspaceHitsDto {
    fn from(group: &WorkspaceHits) -> Self {
        Self {
            workspace: group.workspace.to_string(),
            hits: group.hits.iter().map(HitDto::from).collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchResponse {
    Grouped { groups: Vec<WorkspaceHitsDto> },
    Flat { hits: Vec<HitDto> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DigestEntryDto {
    pub workspace: String,
    #[serde(flatten)]
    pub memory: MemoryDto,
}

impl From<&DigestEntry> for DigestEntryDto {
    fn from(entry: &DigestEntry) -> Self {
        Self {
            workspace: entry.workspace.to_string(),
            memory: MemoryDto::from(&entry.memory),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextResponse {
    pub pinned: Vec<DigestEntryDto>,
    pub recent_project: Vec<DigestEntryDto>,
    pub shared_user: Vec<DigestEntryDto>,
}

impl From<&ContextDigest> for ContextResponse {
    fn from(digest: &ContextDigest) -> Self {
        Self {
            pinned: digest.pinned.iter().map(DigestEntryDto::from).collect(),
            recent_project: digest
                .recent_project
                .iter()
                .map(DigestEntryDto::from)
                .collect(),
            shared_user: digest
                .shared_user
                .iter()
                .map(DigestEntryDto::from)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub memories: Vec<MemoryDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportDoc {
    pub version: u32,
    pub workspace: String,
    pub memories: Vec<MemoryDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportReport {
    pub imported: usize,
    pub unchanged: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceDto {
    pub workspace: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspacesResponse {
    pub workspaces: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
