use std::fmt;
use std::time::Duration;

use api::{
    ContextResponse, ExportDoc, HealthResponse, ImportReport, ListResponse, MemoryDto, MoveBody,
    MoveResponse, PatchMemoryBody, PutMemoryBody, PutPreferenceBody, SearchResponse, WorkspaceDto,
    WorkspacesResponse,
};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8737";

#[derive(Clone, Debug)]
pub enum ClientError {
    Transport(String),
    Status { status: u16, message: String },
    Decode(String),
}

impl ClientError {
    /// Only a definitive rejection is safe to give up on. An unreadable 2xx may sit on top
    /// of a committed write, and replaying it under the same id is cheaper than a duplicate.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Decode(_) => true,
            Self::Status { status, .. } => *status >= 500 || *status == 408 || *status == 429,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "cannot reach synapse server: {msg}"),
            Self::Status { status, message } => write!(f, "server returned {status}: {message}"),
            Self::Decode(msg) => write!(f, "unreadable server response: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<reqwest::Error> for ClientError {
    fn from(err: reqwest::Error) -> Self {
        // `without_url` keeps the queued content and workspace out of the error line.
        let cause = root_cause(&err);
        let summary = err.without_url().to_string();
        Self::Transport(match cause {
            Some(cause) => format!("{summary}: {cause}"),
            None => summary,
        })
    }
}

fn root_cause(err: &dyn std::error::Error) -> Option<String> {
    let mut current = err.source()?;
    while let Some(next) = current.source() {
        current = next;
    }
    Some(current.to_string())
}

pub struct SynapseApiClient {
    http: Client,
    base: String,
}

impl SynapseApiClient {
    pub fn new(base_url: &str, token: &str, timeout: Duration) -> Result<Self, ClientError> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ClientError::Transport("token contains invalid characters".into()))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let http = Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .build()?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn health(&self) -> Result<HealthResponse, ClientError> {
        let response = self.http.get(self.url("/health")).send()?;
        let status = response.status().as_u16();
        let body: HealthResponse = response
            .json()
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        if status == 200 || status == 503 {
            Ok(body)
        } else {
            Err(ClientError::Status {
                status,
                message: body.reason.unwrap_or(body.status),
            })
        }
    }

    pub fn create_workspace(&self, workspace: &str) -> Result<WorkspaceDto, ClientError> {
        json(self.http.put(self.url(&format!("/workspaces/{workspace}"))))
    }

    pub fn workspaces(&self) -> Result<Vec<String>, ClientError> {
        let body: WorkspacesResponse = json(self.http.get(self.url("/workspaces")))?;
        Ok(body.workspaces)
    }

    pub fn save(
        &self,
        workspace: &str,
        id: &str,
        body: &PutMemoryBody,
    ) -> Result<MemoryDto, ClientError> {
        json(
            self.http
                .put(self.url(&format!("/memories/{id}")))
                .query(&[("ws", workspace)])
                .json(body),
        )
    }

    pub fn save_preference(
        &self,
        id: &str,
        body: &PutPreferenceBody,
    ) -> Result<MemoryDto, ClientError> {
        json(
            self.http
                .put(self.url(&format!("/preferences/{id}")))
                .json(body),
        )
    }

    pub fn edit_preference(
        &self,
        id: &str,
        body: &PatchMemoryBody,
    ) -> Result<MemoryDto, ClientError> {
        json(
            self.http
                .patch(self.url(&format!("/preferences/{id}")))
                .json(body),
        )
    }

    pub fn forget_preference(&self, id: &str) -> Result<(), ClientError> {
        let response = self
            .http
            .delete(self.url(&format!("/preferences/{id}")))
            .send()?;
        check(response).map(drop)
    }

    pub fn get_preference(&self, id: &str) -> Result<MemoryDto, ClientError> {
        json(self.http.get(self.url(&format!("/preferences/{id}"))))
    }

    pub fn list_preferences(&self) -> Result<Vec<MemoryDto>, ClientError> {
        let body: ListResponse = json(self.http.get(self.url("/preferences")))?;
        Ok(body.memories)
    }

    pub fn export_preferences(&self) -> Result<ExportDoc, ClientError> {
        json(self.http.get(self.url("/preferences/export")))
    }

    pub fn import_preferences(
        &self,
        merge: bool,
        doc: &ExportDoc,
    ) -> Result<ImportReport, ClientError> {
        json(
            self.http
                .post(self.url("/preferences/import"))
                .query(&[("mode", import_mode(merge))])
                .json(doc),
        )
    }

    pub fn search(
        &self,
        workspace: &str,
        query: &str,
        scope: Option<&str>,
        limit: usize,
        all_workspaces: bool,
        links_in_scope: bool,
    ) -> Result<SearchResponse, ClientError> {
        let limit = limit.to_string();
        let mut params: Vec<(&str, &str)> = vec![("q", query), ("limit", &limit)];
        if all_workspaces {
            params.push(("all", "true"));
        } else {
            params.push(("ws", workspace));
        }
        if let Some(scope) = scope {
            params.push(("scope", scope));
        }
        if links_in_scope {
            params.push(("links_scope", "true"));
        }
        json(self.http.get(self.url("/memories/search")).query(&params))
    }

    pub fn context(
        &self,
        workspace: &str,
        project: Option<&str>,
    ) -> Result<ContextResponse, ClientError> {
        let mut params: Vec<(&str, &str)> = vec![("ws", workspace)];
        if let Some(project) = project {
            params.push(("project", project));
        }
        json(self.http.get(self.url("/context")).query(&params))
    }

    pub fn edit(
        &self,
        workspace: &str,
        id: &str,
        body: &PatchMemoryBody,
    ) -> Result<MemoryDto, ClientError> {
        json(
            self.http
                .patch(self.url(&format!("/memories/{id}")))
                .query(&[("ws", workspace)])
                .json(body),
        )
    }

    pub fn forget(&self, workspace: &str, id: &str) -> Result<(), ClientError> {
        let response = self
            .http
            .delete(self.url(&format!("/memories/{id}")))
            .query(&[("ws", workspace)])
            .send()?;
        check(response).map(drop)
    }

    pub fn move_memory(&self, id: &str, body: &MoveBody) -> Result<MoveResponse, ClientError> {
        json(
            self.http
                .post(self.url(&format!("/memories/{id}/move")))
                .json(body),
        )
    }

    pub fn get(&self, workspace: &str, id: &str) -> Result<MemoryDto, ClientError> {
        json(
            self.http
                .get(self.url(&format!("/memories/{id}")))
                .query(&[("ws", workspace)]),
        )
    }

    pub fn list(&self, workspace: &str) -> Result<Vec<MemoryDto>, ClientError> {
        let body: ListResponse = json(
            self.http
                .get(self.url("/memories"))
                .query(&[("ws", workspace)]),
        )?;
        Ok(body.memories)
    }

    pub fn export(&self, workspace: &str) -> Result<ExportDoc, ClientError> {
        json(
            self.http
                .get(self.url("/export"))
                .query(&[("ws", workspace)]),
        )
    }

    pub fn import(
        &self,
        workspace: &str,
        merge: bool,
        doc: &ExportDoc,
    ) -> Result<ImportReport, ClientError> {
        json(
            self.http
                .post(self.url("/import"))
                .query(&[("ws", workspace), ("mode", import_mode(merge))])
                .json(doc),
        )
    }
}

fn import_mode(merge: bool) -> &'static str {
    if merge { "merge" } else { "fail-if-nonempty" }
}

fn json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, ClientError> {
    let body = check(request.send()?)?;
    serde_json::from_slice(&body).map_err(|e| ClientError::Decode(e.to_string()))
}

fn check(response: reqwest::blocking::Response) -> Result<Vec<u8>, ClientError> {
    let status = response.status().as_u16();
    let body = response.bytes()?.to_vec();
    if (200..300).contains(&status) {
        return Ok(body);
    }
    Err(ClientError::Status {
        status,
        message: error_message(&body),
    })
}

fn error_message(body: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: String,
    }
    serde_json::from_slice::<ErrorBody>(body)
        .map(|e| e.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(body).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_splits_on_status_class() {
        let transport = ClientError::Transport("connection refused".into());
        assert!(transport.is_retryable());
        assert!(
            ClientError::Decode("expected value at line 1".into()).is_retryable(),
            "an unreadable 2xx may follow a committed write"
        );
        for status in [500, 502, 503, 408, 429] {
            assert!(
                ClientError::Status {
                    status,
                    message: String::new()
                }
                .is_retryable(),
                "{status} should retry"
            );
        }
        for status in [400, 401, 404, 409, 413] {
            assert!(
                !ClientError::Status {
                    status,
                    message: String::new()
                }
                .is_retryable(),
                "{status} should not retry"
            );
        }
    }

    #[test]
    fn error_message_prefers_api_error_field() {
        assert_eq!(error_message(br#"{"error":"bad tag"}"#), "bad tag");
        assert_eq!(error_message(b"plain text\n"), "plain text");
    }
}
