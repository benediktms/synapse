use std::io;
use std::sync::{Arc, Mutex};

use adapters_fastembed::FastEmbedder;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use server::App;
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::fmt::format::FmtSpan;

const TOKEN: &str = "s3cr3t-bearer-token";
const PRIVATE_QUERY: &str = "my-therapist-appointment";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn request_logs_carry_neither_the_query_string_nor_the_bearer_token() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .init();

    let dir = TempDir::new().unwrap();
    let embedder = tokio::task::spawn_blocking(|| Arc::new(FastEmbedder::new().expect("model")))
        .await
        .expect("join");
    let app = App::boot(dir.path().to_path_buf(), embedder)
        .await
        .expect("boot");
    let router = api::router(app, TOKEN);

    let create = Request::builder()
        .method(Method::PUT)
        .uri("/workspaces/work")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let status = router.clone().oneshot(create).await.unwrap().status();
    assert_eq!(status, StatusCode::CREATED);

    let search = Request::builder()
        .method(Method::GET)
        .uri(format!("/memories/search?ws=work&q={PRIVATE_QUERY}"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let status = router.clone().oneshot(search).await.unwrap().status();
    assert_eq!(status, StatusCode::OK);

    let logs = capture.text();
    assert!(
        logs.contains("/memories/search"),
        "the request was never logged at all: {logs}"
    );
    for secret in [PRIVATE_QUERY, TOKEN, "?ws=", "authorization", "Bearer"] {
        assert!(
            !logs.contains(secret),
            "request log leaks {secret:?}:\n{logs}"
        );
    }
}
