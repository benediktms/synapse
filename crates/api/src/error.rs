use crate::backend::BackendError;

#[derive(Clone, Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    NotFound(String),
    Conflict(String),
    Unready(String),
    Internal(String),
}

impl From<BackendError> for ApiError {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::UnknownWorkspace(ws) => {
                Self::NotFound(format!("unknown workspace: {ws}"))
            }
            BackendError::Domain(err) => err.into(),
        }
    }
}

impl From<domain::Error> for ApiError {
    fn from(err: domain::Error) -> Self {
        use domain::Error;
        match &err {
            Error::NotFound(_) => Self::NotFound(err.to_string()),
            Error::Conflict(_) => Self::Conflict(err.to_string()),
            Error::Store(_) | Error::Embed(_) => Self::Internal(err.to_string()),
            _ => Self::BadRequest(err.to_string()),
        }
    }
}
