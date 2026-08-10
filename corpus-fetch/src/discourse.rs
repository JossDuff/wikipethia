//! ethresear.ch endpoints and the minimum JSON navigation needed to walk them.
//!
//! Everything here follows NOTES-discourse-api.md, verified against the live
//! instance (Discourse 3.5.2). The stored payload stays an untyped
//! [`Value`] — M1 stores raw JSON; parsing into `Document` is M2. The serde
//! structs below only peek at the fields the walk itself needs.

use serde::Deserialize;
use serde_json::Value;

use crate::error::FetchError;

/// Post IDs per `post_ids[]` batch request. The server cap is URL length
/// (~8 KB → HTTP 414), not an ID count; 150 six-digit IDs is ~3 KB.
pub const BATCH_SIZE: usize = 150;

pub fn latest_url(base: &str, page: u32) -> String {
    format!("{base}/latest.json?no_definitions=true&page={page}")
}

/// Instance stats — one request at sync start, only for progress display.
pub fn about_url(base: &str) -> String {
    format!("{base}/about.json")
}

/// `include_raw=1`: `raw` markdown is absent from every payload unless asked
/// for. Costs no extra requests.
pub fn topic_url(base: &str, topic_id: u64) -> String {
    format!("{base}/t/{topic_id}.json?include_raw=1")
}

/// `post_ids[]` brackets are percent-encoded to keep the URI parser happy;
/// Rails decodes them identically.
pub fn posts_batch_url(base: &str, topic_id: u64, post_ids: &[u64]) -> String {
    let mut url = format!("{base}/t/{topic_id}/posts.json?");
    for id in post_ids {
        url.push_str(&format!("post_ids%5B%5D={id}&"));
    }
    url.push_str("include_raw=1");
    url
}

/// One page of `/latest.json`.
#[derive(Debug, Deserialize)]
pub struct LatestPage {
    pub topic_list: TopicList,
}

#[derive(Debug, Deserialize)]
pub struct TopicList {
    /// Termination signal: `null` past the last page (the response is still
    /// HTTP 200 — never terminate on status code).
    #[serde(default)]
    pub more_topics_url: Option<String>,
    #[serde(default)]
    pub topics: Vec<TopicSummary>,
}

#[derive(Debug, Deserialize)]
pub struct TopicSummary {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub posts_count: u64,
}

/// All post IDs in the topic, in thread order. `post_stream.posts` holds only
/// the first `chunk_size` (20) posts; this is the full list.
pub fn stream_ids(topic: &Value) -> Result<Vec<u64>, FetchError> {
    let stream = topic
        .pointer("/post_stream/stream")
        .and_then(Value::as_array)
        .ok_or_else(|| FetchError::Shape("topic JSON has no post_stream.stream".into()))?;
    stream
        .iter()
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| FetchError::Shape(format!("non-integer id in stream: {v}")))
        })
        .collect()
}

fn loaded_posts(topic: &Value) -> Option<&Vec<Value>> {
    topic.pointer("/post_stream/posts").and_then(Value::as_array)
}

/// IDs in the stream that are not among the posts already inlined in the
/// topic payload, in stream order. These need `post_ids[]` follow-ups.
pub fn missing_post_ids(topic: &Value) -> Result<Vec<u64>, FetchError> {
    let loaded: Vec<u64> = loaded_posts(topic)
        .map(|posts| posts.iter().filter_map(|p| p["id"].as_u64()).collect())
        .unwrap_or_default();
    Ok(stream_ids(topic)?
        .into_iter()
        .filter(|id| !loaded.contains(id))
        .collect())
}

/// Extract the posts from a `/t/{id}/posts.json?post_ids[]=` response.
/// Deleted/unknown IDs are silently dropped by the server, so this may hold
/// fewer posts than were requested — that is normal, not an error.
pub fn batch_posts(mut response: Value) -> Result<Vec<Value>, FetchError> {
    match response.pointer_mut("/post_stream/posts").map(Value::take) {
        Some(Value::Array(posts)) => Ok(posts),
        _ => Err(FetchError::Shape(
            "batch response has no post_stream.posts array".into(),
        )),
    }
}

/// Append batch-fetched posts into `post_stream.posts`, dedup by post id, and
/// restore thread order (`post_number`, which has gaps where posts were
/// deleted — sort, don't assume contiguity).
pub fn merge_posts(topic: &mut Value, new_posts: Vec<Value>) -> Result<(), FetchError> {
    let posts = topic
        .pointer_mut("/post_stream/posts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| FetchError::Shape("topic JSON has no post_stream.posts".into()))?;
    let existing: Vec<u64> = posts.iter().filter_map(|p| p["id"].as_u64()).collect();
    posts.extend(
        new_posts
            .into_iter()
            .filter(|p| !p["id"].as_u64().is_some_and(|id| existing.contains(&id))),
    );
    posts.sort_by_key(|p| {
        (
            p["post_number"].as_u64().unwrap_or(u64::MAX),
            p["id"].as_u64().unwrap_or(u64::MAX),
        )
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_url_stays_far_under_the_8kb_server_cap() {
        // Worst realistic case: a full batch of 7-digit post IDs.
        let ids: Vec<u64> = (0..BATCH_SIZE as u64).map(|i| 9_000_000 + i).collect();
        let url = posts_batch_url("https://ethresear.ch", 9_999_999, &ids);
        assert!(url.len() < 4096, "batch URL is {} bytes", url.len());
        assert!(url.ends_with("include_raw=1"));
        assert_eq!(url.matches("post_ids%5B%5D=").count(), BATCH_SIZE);
    }
}
