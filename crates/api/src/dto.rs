use domain::{
    ContextDigest, DigestEntry, Memory, MemoryId, MemoryKind, RecallHit, Scope, Timestamp,
    Workspace, WorkspaceHits,
};
use serde::{Deserialize, Serialize};

pub const EXPORT_VERSION: u32 = 1;

/// Where a memory came from, as clients are allowed to see it. The shared database
/// backing preferences is a storage detail and is never named on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Preference,
    Workspace(String),
}

impl Origin {
    pub fn of(workspace: &Workspace) -> Self {
        if workspace.is_shared() {
            Self::Preference
        } else {
            Self::Workspace(workspace.to_string())
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Preference => "preference",
            Self::Workspace(name) => name,
        }
    }
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutPreferenceBody {
    pub content: String,
    pub kind: String,
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
    pub origin: Origin,
    pub score: f64,
    #[serde(flatten)]
    pub memory: MemoryDto,
}

impl From<&RecallHit> for HitDto {
    fn from(hit: &RecallHit) -> Self {
        Self {
            origin: Origin::of(&hit.workspace),
            score: hit.score,
            memory: MemoryDto::from(&hit.memory),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitGroupDto {
    pub origin: Origin,
    pub hits: Vec<HitDto>,
}

impl From<&WorkspaceHits> for HitGroupDto {
    fn from(group: &WorkspaceHits) -> Self {
        Self {
            origin: Origin::of(&group.workspace),
            hits: group.hits.iter().map(HitDto::from).collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchResponse {
    Grouped { groups: Vec<HitGroupDto> },
    Flat { hits: Vec<HitDto> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DigestEntryDto {
    pub origin: Origin,
    #[serde(flatten)]
    pub memory: MemoryDto,
}

impl From<&DigestEntry> for DigestEntryDto {
    fn from(entry: &DigestEntry) -> Self {
        Self {
            origin: Origin::of(&entry.workspace),
            memory: MemoryDto::from(&entry.memory),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextResponse {
    pub pinned: Vec<DigestEntryDto>,
    pub recent_project: Vec<DigestEntryDto>,
    pub preferences: Vec<DigestEntryDto>,
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
            preferences: digest
                .preferences
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
    pub origin: Origin,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> MemoryDto {
        MemoryDto {
            id: "m_0000000000000000000001".into(),
            content: "a fact".into(),
            kind: "user".into(),
            scope: "workspace".into(),
            tags: vec![],
            pinned: false,
            created_at: "2026-08-02T10:00:00Z".into(),
            updated_at: "2026-08-02T10:00:00Z".into(),
        }
    }

    #[test]
    fn shared_becomes_preference_and_never_names_the_backing_workspace() {
        assert_eq!(Origin::of(&Workspace::shared()), Origin::Preference);
        assert_eq!(
            Origin::of(&Workspace::new("work").unwrap()),
            Origin::Workspace("work".into())
        );
        let json = serde_json::to_string(&HitDto {
            origin: Origin::Preference,
            score: 0.5,
            memory: memory(),
        })
        .unwrap();
        assert!(json.contains(r#""origin":"preference""#), "{json}");
        assert!(!json.contains("shared"), "{json}");
    }

    #[test]
    fn hits_survive_a_wire_roundtrip_alongside_the_flattened_memory() {
        for origin in [Origin::Preference, Origin::Workspace("work".into())] {
            let hit = HitDto {
                origin: origin.clone(),
                score: 0.75,
                memory: memory(),
            };
            let text = serde_json::to_string(&hit).unwrap();
            let back: HitDto = serde_json::from_str(&text).unwrap();
            assert_eq!(back.origin, origin);
            assert_eq!(back.score, 0.75);
            assert_eq!(back.memory.id, hit.memory.id);
        }
    }
}
