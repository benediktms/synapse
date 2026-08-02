use std::sync::{Arc, Mutex};

use domain::{Embedder, Error};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use tokio::sync::Semaphore;

pub const MODEL_NAME: &str = "bge-small-en-v1.5";
pub const DIMENSION: usize = 384;

pub struct FastEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
    permits: Arc<Semaphore>,
}

impl FastEmbedder {
    pub fn new() -> Result<Self, Error> {
        let options =
            TextInitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false);
        let model = TextEmbedding::try_new(options).map_err(|e| Error::Embed(e.to_string()))?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            // ponytail: TextEmbedding::embed needs &mut, so one permit = one ONNX
            // session, serialized; raise permits with a session pool if throughput matters
            permits: Arc::new(Semaphore::new(1)),
        })
    }

    pub fn model_name(&self) -> &'static str {
        MODEL_NAME
    }

    pub fn dimension(&self) -> usize {
        DIMENSION
    }
}

impl Embedder for FastEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, Error> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?;
        let model = Arc::clone(&self.model);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| Error::Embed("embedder mutex poisoned".into()))?;
            let mut embeddings = model
                .embed([text], None)
                .map_err(|e| Error::Embed(e.to_string()))?;
            embeddings
                .pop()
                .ok_or_else(|| Error::Embed("model returned no embedding".into()))
        })
        .await
        .map_err(|e| Error::Embed(e.to_string()))?
    }
}
