mod dto;

pub mod limits;
pub mod rpc;

#[cfg(feature = "server")]
mod backend;
#[cfg(feature = "server")]
mod error;
#[cfg(feature = "server")]
pub mod ops;
#[cfg(feature = "server")]
mod routes;
#[cfg(feature = "server")]
mod validate;

pub use dto::{
    ContextResponse, DigestEntryDto, EXPORT_VERSION, ExportDoc, GraphDto, HealthResponse, HitDto,
    HitGroupDto, ImportReport, LinkDto, ListResponse, MemoryDto, MoveBody, MoveResponse,
    NeighborDto, Origin, PatchMemoryBody, PutMemoryBody, PutPreferenceBody, SearchResponse,
    WorkspaceDto, WorkspacesResponse,
};

#[cfg(feature = "server")]
pub use backend::{Backend, BackendError, RestoreReport};
#[cfg(feature = "server")]
pub use error::ApiError;
pub use limits::{CONTENT_MAX_BYTES, MAX_TAGS, QUERY_MAX_BYTES, TAG_MAX_BYTES};
#[cfg(feature = "server")]
pub use routes::router;
