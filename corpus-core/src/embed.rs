//! The embedding abstraction. Implementations live outside this crate —
//! the real one (fastembed) in `corpus-embed`, deterministic fakes in tests.
//! corpus-core itself never loads a model; it only stores and searches the
//! vectors an [`Embedder`] produces.

use crate::error::CoreError;

pub trait Embedder {
    /// Stable model identifier (e.g. `BAAI/bge-small-en-v1.5`), recorded in
    /// the store so a model change can be detected and force a re-embed.
    fn id(&self) -> &str;

    /// Output vector length. Fixed per model; baked into the vector table.
    fn dimension(&self) -> usize;

    /// Embed passages for indexing. One vector per input text.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError>;

    /// Embed a search query. Models with an asymmetric retrieval convention
    /// (BGE prefixes queries, not passages) override this; the default is a
    /// plain passage embedding.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, CoreError> {
        Ok(self.embed(&[text])?.pop().expect("one vector per text"))
    }
}
