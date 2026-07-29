use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The source-agnostic unit of the corpus: a Discourse post, an EIP markdown
/// file, a blog article, a PDF section. Anything only one source type can
/// populate (`post_number`, `topic_id`, `username`) belongs in [`meta`],
/// never as a field.
///
/// [`meta`]: Document::meta
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Stable unique id, e.g. `ethresearch/post/12345`.
    pub id: String,
    /// Source id, e.g. `ethresearch`.
    pub source: String,
    /// Canonical absolute URL.
    pub url: String,
    /// Title of the containing work (topic title, EIP title, article title).
    pub title: String,
    pub author: Option<String>,
    /// ISO-8601 UTC as emitted by the source. Sorts lexicographically, so no
    /// date crate is needed until something stops emitting ISO-8601.
    pub published: String,
    /// Cleaned markdown.
    pub content: String,
    /// Source-specific fields, stored as JSON.
    pub meta: Map<String, Value>,
}
