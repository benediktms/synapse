use domain::{
    ContextDigest, DigestEntry, GraphEdge, GraphNode, GraphSubgraph, Memory, MemoryId, MemoryKind,
    RecallHit, RecallLink, Scope, Timestamp, Workspace, WorkspaceHits,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const EXPORT_VERSION: u32 = 2;

fn default_tier() -> String {
    domain::Importance::DEFAULT.as_str().to_string()
}

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
    #[serde(default = "default_tier")]
    pub importance: String,
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
            importance: memory.importance.as_str().to_string(),
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
            importance: domain::Importance::parse(&self.importance)?,
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
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutPreferenceBody {
    pub content: String,
    pub kind: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoveBody {
    pub from: Origin,
    pub to: Origin,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoveResponse {
    pub moved: bool,
    pub from: Origin,
    pub to: Origin,
    pub from_scope: String,
    #[serde(default)]
    pub links_dropped: usize,
    #[serde(flatten)]
    pub memory: MemoryDto,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PatchMemoryBody {
    pub content: Option<String>,
    pub tags: Option<Vec<String>>,
    pub pinned: Option<bool>,
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitDto {
    pub origin: Origin,
    pub score: f64,
    #[serde(flatten)]
    pub memory: MemoryDto,
    /// First-hop linked neighbors, for graph surfacing. Ids and phrases only.
    #[serde(default)]
    pub neighbors: Vec<NeighborDto>,
    /// More first-hop neighbors exist than `neighbors` carries; walk them with the links route.
    #[serde(default)]
    pub neighbors_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeighborDto {
    pub id: String,
    pub phrase: String,
    pub scope: String,
}

impl From<&RecallLink> for NeighborDto {
    fn from(link: &RecallLink) -> Self {
        Self {
            id: link.id.to_string(),
            phrase: link.phrase.clone(),
            scope: link.scope.as_str().to_string(),
        }
    }
}

impl From<&RecallHit> for HitDto {
    fn from(hit: &RecallHit) -> Self {
        Self {
            origin: Origin::of(&hit.workspace),
            score: hit.score,
            memory: MemoryDto::from(&hit.memory),
            neighbors: hit.links.iter().map(NeighborDto::from).collect(),
            neighbors_truncated: hit.links_truncated,
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

/// A link in an export dump. Endpoints are the canonical stored form (symmetric links
/// low-id first; supersession in true source->target orientation).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkDto {
    pub source: String,
    pub relation: String,
    pub target: String,
    pub directed: bool,
}

impl From<&GraphEdge> for LinkDto {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            source: edge.source.to_string(),
            relation: edge.relation.as_str().to_string(),
            target: edge.target.to_string(),
            directed: edge.directed,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportDoc {
    pub version: u32,
    pub origin: Origin,
    pub memories: Vec<MemoryDto>,
    #[serde(default)]
    pub links: Vec<LinkDto>,
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

/// A graph dump in JSON Graph Format v2. Truncation (not part of the JGF spec) rides on the graph
/// container as `metadata`; per-node truncation as `metadata`. The JGF v2 schema pins `directed`,
/// `nodes`, `edges` and `metadata` at graph level — traversal fields like root/depth are not legal
/// there, so they ride inside `metadata`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphDto {
    pub graph: GraphContainerDto,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphContainerDto {
    pub directed: bool,
    pub nodes: BTreeMap<String, GraphNodeDto>,
    pub edges: Vec<GraphEdgeDto>,
    #[serde(default)]
    pub metadata: GraphMetaDto,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphMetaDto {
    pub root: String,
    pub depth: usize,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphNodeDto {
    pub label: String,
    #[serde(default = "default_node_meta")]
    pub metadata: GraphNodeMetaDto,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphNodeMetaDto {
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphEdgeDto {
    pub source: String,
    /// The edge type noun: relation / support / contradiction / supersession.
    pub relation: String,
    pub target: String,
    #[serde(default)]
    pub directed: bool,
}

fn default_node_meta() -> GraphNodeMetaDto {
    GraphNodeMetaDto::default()
}

impl From<&GraphNode> for GraphNodeDto {
    fn from(node: &GraphNode) -> Self {
        Self {
            label: node.memory.content.clone(),
            metadata: GraphNodeMetaDto {
                truncated: node.truncated,
            },
        }
    }
}

impl From<&GraphEdge> for GraphEdgeDto {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            source: edge.source.to_string(),
            relation: edge.relation.as_str().to_string(),
            target: edge.target.to_string(),
            directed: edge.directed,
        }
    }
}

impl From<&GraphSubgraph> for GraphDto {
    fn from(sub: &GraphSubgraph) -> Self {
        Self {
            graph: GraphContainerDto {
                directed: sub.edges.iter().any(|e| e.directed),
                nodes: sub
                    .nodes
                    .iter()
                    .map(|n| (n.memory.id.to_string(), GraphNodeDto::from(n)))
                    .collect(),
                edges: sub.edges.iter().map(GraphEdgeDto::from).collect(),
                metadata: GraphMetaDto {
                    root: sub.root.to_string(),
                    depth: sub.depth,
                    truncated: sub.truncated,
                },
            },
        }
    }
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
            importance: "medium".into(),
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
            neighbors: vec![],
            neighbors_truncated: false,
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
                neighbors: vec![],
                neighbors_truncated: false,
            };
            let text = serde_json::to_string(&hit).unwrap();
            let back: HitDto = serde_json::from_str(&text).unwrap();
            assert_eq!(back.origin, origin);
            assert_eq!(back.score, 0.75);
            assert_eq!(back.memory.id, hit.memory.id);
        }
    }

    #[test]
    fn graph_dto_serializes_jgf_v2_shape_with_meta_and_truncation() {
        let mid = |n: u32| MemoryId::parse(&format!("m_{n:022}")).unwrap();
        let node = |n: u32, truncated: bool| GraphNode {
            memory: Memory {
                id: mid(n),
                content: format!("memory {n}"),
                kind: MemoryKind::Project,
                scope: Scope::Workspace,
                tags: vec![],
                pinned: false,
                importance: domain::Importance::DEFAULT,
                created_at: Timestamp::new("2026-08-02T10:00:00Z"),
                updated_at: Timestamp::new("2026-08-02T10:00:00Z"),
            },
            truncated,
        };
        let sub = GraphSubgraph {
            root: mid(1),
            depth: 2,
            truncated: true,
            nodes: vec![node(1, false), node(2, true)],
            edges: vec![GraphEdge {
                source: mid(1),
                target: mid(2),
                relation: domain::Relation::Supersession,
                directed: true,
            }],
        };
        let json = serde_json::to_value(GraphDto::from(&sub)).unwrap();
        assert_eq!(
            json["graph"]["metadata"]["root"],
            "m_0000000000000000000001"
        );
        assert_eq!(json["graph"]["metadata"]["depth"], 2);
        assert_eq!(json["graph"]["metadata"]["truncated"], true);
        assert_eq!(json["graph"]["directed"], true);
        assert_eq!(
            json["graph"]["nodes"]["m_0000000000000000000002"]["metadata"]["truncated"],
            true
        );
        assert_eq!(json["graph"]["edges"][0]["relation"], "supersession");
        assert_eq!(json["graph"]["edges"][0]["directed"], true);
        assert!(json["graph"]["root"].is_null());
        assert!(json["graph"]["depth"].is_null());
        assert!(json["graph"]["truncated"].is_null());
    }
}
