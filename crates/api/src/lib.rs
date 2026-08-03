mod dto;

#[cfg(feature = "server")]
mod backend;
#[cfg(feature = "server")]
mod error;
#[cfg(feature = "server")]
mod routes;
#[cfg(feature = "server")]
mod validate;

pub use dto::{
    ContextResponse, DigestEntryDto, EXPORT_VERSION, ExportDoc, HealthResponse, HitDto,
    HitGroupDto, ImportReport, ListResponse, MemoryDto, MoveBody, MoveResponse, Origin,
    PatchMemoryBody, PutMemoryBody, PutPreferenceBody, SearchResponse, WorkspaceDto,
    WorkspacesResponse,
};

#[cfg(feature = "server")]
pub use backend::{Backend, BackendError, RestoreReport};
#[cfg(feature = "server")]
pub use error::ApiError;
#[cfg(feature = "server")]
pub use routes::router;
#[cfg(feature = "server")]
pub use validate::{CONTENT_MAX_BYTES, MAX_TAGS, QUERY_MAX_BYTES, TAG_MAX_BYTES};
