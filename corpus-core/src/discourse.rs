//! Discourse topic JSON → [`Document`]s. Deliberately a concrete module, not
//! an adapter trait — the trait arrives at M6 with a second call site.
//!
//! Input is the self-contained topic JSON that `corpus-fetch` writes: every
//! still-existing post inlined in `post_stream.posts` with `raw` present.
//! `post_number` has gaps where posts were deleted; that is normal here, not
//! an error.

use serde_json::{Map, Value};

use crate::clean::strip_quote_blocks;
use crate::document::Document;
use crate::error::CoreError;

pub const SOURCE: &str = "ethresearch";

/// Regular user post. Other types (moderator action, small-action, whisper)
/// are bookkeeping, not research — indexing them would pollute the corpus.
const POST_TYPE_REGULAR: u64 = 1;

/// Parse one topic's JSON into one [`Document`] per regular post.
pub fn parse_topic(topic: &Value, base_url: &str) -> Result<Vec<Document>, CoreError> {
    let topic_id = topic["id"]
        .as_u64()
        .ok_or_else(|| CoreError::Parse("topic has no id".into()))?;
    let title = topic["title"]
        .as_str()
        .ok_or_else(|| CoreError::Parse(format!("topic {topic_id} has no title")))?;
    let posts = topic
        .pointer("/post_stream/posts")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Parse(format!("topic {topic_id} has no post_stream.posts")))?;

    let base = base_url.trim_end_matches('/');
    let mut docs = Vec::with_capacity(posts.len());
    let mut raw_missing = 0usize;
    for post in posts {
        if post["post_type"].as_u64() != Some(POST_TYPE_REGULAR) {
            continue;
        }
        let post_id = post["id"]
            .as_u64()
            .ok_or_else(|| CoreError::Parse(format!("post in topic {topic_id} has no id")))?;
        // Even with include_raw=1 the server omits raw for a few degenerate
        // posts (suspended-user remnants — live example: post 11811 in
        // topic 465). Skip those; the every-post check below still catches
        // a topic fetched without include_raw=1 at all.
        let Some(raw) = post["raw"].as_str() else {
            raw_missing += 1;
            continue;
        };
        let post_number = post["post_number"]
            .as_u64()
            .ok_or_else(|| CoreError::Parse(format!("post {post_id} has no post_number")))?;
        let published = post["created_at"]
            .as_str()
            .ok_or_else(|| CoreError::Parse(format!("post {post_id} has no created_at")))?;
        let url = match post["post_url"].as_str() {
            Some(path) => format!("{base}{path}"),
            None => format!("{base}/t/{topic_id}/{post_number}"),
        };

        let mut meta = Map::new();
        meta.insert("topic_id".into(), topic_id.into());
        meta.insert("post_number".into(), post_number.into());
        for (key, value) in [
            ("category_id", &topic["category_id"]),
            ("tags", &topic["tags"]),
            ("reply_to_post_number", &post["reply_to_post_number"]),
            ("accepted_answer", &post["accepted_answer"]),
        ] {
            if !value.is_null() {
                meta.insert(key.into(), value.clone());
            }
        }

        docs.push(Document {
            id: format!("{SOURCE}/post/{post_id}"),
            source: SOURCE.into(),
            url,
            title: title.into(),
            author: post["username"].as_str().map(String::from),
            published: published.into(),
            content: strip_quote_blocks(raw),
            meta,
        });
    }
    if docs.is_empty() && raw_missing > 0 {
        return Err(CoreError::Parse(format!(
            "no post in topic {topic_id} has raw (fetched without include_raw=1?)"
        )));
    }
    Ok(docs)
}
