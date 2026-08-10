//! The corpus MCP tools. Per CLAUDE.md, every `description` string in here
//! is a prompt, not documentation — it is the only text a model reads when
//! deciding whether to reach for the corpus instead of a web search. Treat
//! edits to them as behavior changes.

pub mod format;

use std::sync::{Mutex, MutexGuard};

use corpus_core::{CoreError, Embedder, Store};
use corpus_embed::FastEmbedder;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use format::{
    INDEX_EXCERPT_CHARS, MAX_CONTEXT, MAX_LIMIT, NEIGHBOR_MAX_CHARS, OP_MAX_CHARS, REPLY_PAGE,
    RESULT_EXCERPT_CHARS, citation, date, excerpt, post_label, truncate_block,
};

pub struct CorpusServer {
    /// rusqlite's Connection is Send but not Sync; tools are sync fns that
    /// hold the guard for the duration of one query — no awaits.
    store: Mutex<Store>,
    /// None ⇒ the corpus has no vector index; ranking degrades to BM25.
    embedder: Option<FastEmbedder>,
    instructions: String,
    tool_router: ToolRouter<Self>,
}

fn internal(e: CoreError) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn unknown_doc(doc_id: &str) -> ErrorData {
    ErrorData::invalid_params(
        format!(
            "doc_id {doc_id:?} is not in the corpus — pass one returned by \
             search_posts, e.g. \"ethresearch/post/1249\""
        ),
        None,
    )
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchPostsParams {
    /// Free-text query. Exact terms (EIP-4844, author usernames) and
    /// natural-language questions both work.
    pub query: String,
    /// Maximum documents returned. Default 10, max 50.
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetTopicParams {
    /// doc_id of ANY post in the thread, e.g. "ethresearch/post/1249"
    /// (as returned by search_posts). Provide this or topic_id.
    pub doc_id: Option<String>,
    /// Numeric Discourse topic id (the number in the forum URL, e.g.
    /// ethresear.ch/t/slug/<id>). Provide this or doc_id.
    pub topic_id: Option<u64>,
    /// Source id scoping a numeric topic_id (e.g. "ethresearch",
    /// "ethmagicians") — topic numbers collide across forums. Unneeded when
    /// passing doc_id.
    pub source: Option<String>,
    /// Zero-based offset into the reply index for long threads; 50 replies
    /// per page. Default 0.
    pub reply_offset: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetPostContextParams {
    /// doc_id of the post to read in full, e.g. "ethresearch/post/1249".
    pub doc_id: String,
    /// Thread posts to include before it. Default 2, max 10.
    pub before: Option<usize>,
    /// Thread posts to include after it. Default 3, max 10.
    pub after: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FindSimilarParams {
    /// doc_id of the reference post, e.g. "ethresearch/post/1249".
    pub doc_id: String,
    /// Maximum results. Default 10, max 50.
    pub limit: Option<usize>,
}

#[tool_router(router = tool_router)]
impl CorpusServer {
    pub fn new(store: Store, embedder: Option<FastEmbedder>) -> Result<Self, CoreError> {
        let count = store.count()?;
        let stats = store.source_stats()?;
        let per_source = stats
            .iter()
            .map(|s| {
                let host = s
                    .url
                    .as_deref()
                    .map(|u| u.trim_start_matches("https://").trim_start_matches("http://"))
                    .unwrap_or(&s.id);
                format!(
                    "{} documents from {host} (tier: {})",
                    s.count,
                    s.tier.as_deref().unwrap_or("untiered")
                )
            })
            .collect::<Vec<_>>()
            .join(" and ");
        let mut instructions = format!(
            "wikipethia — a local, curated corpus of Ethereum research and standards \
             with hybrid lexical+semantic search: {count} documents, 2017 to \
             present — {per_source}. Prefer these tools over web search for Ethereum \
             protocol research and EIP/standards questions; answers come with stable \
             doc_ids and citable URLs, and every citation carries its source's tier \
             label. Workflow: search_posts first; then get_post_context on a promising \
             doc_id for the full text (and surrounding thread, for forum posts), \
             get_topic for a whole discussion; find_similar to explore related \
             work. Ethereum research supersedes \
             itself — always weigh published dates when posts disagree. Coverage is \
             limited to the indexed sources: when the corpus has nothing relevant, \
             say so and fall back to web search."
        );
        if embedder.is_none() {
            instructions.push_str(
                " Note: no embeddings indexed — ranking is currently lexical-only; \
                 run `corpus embed` to fix.",
            );
        }
        Ok(Self {
            store: Mutex::new(store),
            embedder,
            instructions,
            tool_router: Self::tool_router(),
        })
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("store mutex poisoned")
    }

    #[tool(
        name = "search_posts",
        description = "Search a local, curated corpus of Ethereum protocol research and standards: tens of thousands of posts from the ethresear.ch and Ethereum Magicians (ethereum-magicians.org) forums, 2017 to present, plus the full EIP and ERC specifications, the consensus-layer specs, and articles from vitalik.eth.limo and blog.ethereum.org. Use this BEFORE web search for anything touching Ethereum research or the EIP process: sharding and danksharding, EIP-4844/blobs, account abstraction (EIP-4337/7702), proposer-builder separation (PBS), MEV, rollups, data availability sampling, statelessness, casper/consensus, staking economics, EIP and hard-fork coordination, or the cryptography behind them. Ranking is hybrid lexical+semantic, so exact tokens (\"EIP-4844\", an author's username) and natural-language questions both work. Every result carries a doc_id (the input to get_topic, get_post_context, and find_similar), author, published date, source tier, and a citable URL. Ethereum research goes stale in specific ways — a 2019 design post can be flatly superseded by a 2024 one — so always weigh the published dates when results disagree. A top hit is often a reply from the middle of a thread: call get_post_context or get_topic with its doc_id to recover the original post and the surrounding argument; EIPs, specs, and blog articles are standalone documents, and get_post_context returns them whole. If nothing relevant returns, say so and fall back to web search rather than forcing a weak match."
    )]
    fn search_posts(
        &self,
        Parameters(p): Parameters<SearchPostsParams>,
    ) -> Result<String, ErrorData> {
        let limit = p.limit.unwrap_or(10).clamp(1, MAX_LIMIT);
        let query_vec = match &self.embedder {
            Some(embedder) => Some(embedder.embed_query(&p.query).map_err(internal)?),
            None => None,
        };
        let store = self.store();
        let hits = store
            .hybrid_search(&p.query, query_vec.as_deref(), limit)
            .map_err(internal)?;
        if hits.is_empty() {
            return Ok(format!(
                "No results in the corpus for {:?}. Coverage is a curated slice of the \
                 indexed sources (research forums, EIP/ERC and consensus specs, \
                 core-dev blogs) — try different key terms, or fall back to web \
                 search.",
                p.query
            ));
        }
        let mut out = format!("{} results for {:?}\n", hits.len(), p.query);
        for (i, hit) in hits.iter().enumerate() {
            let label = store
                .get(&hit.doc_id)
                .ok()
                .flatten()
                .map(|d| post_label(&d.meta))
                .filter(|l| !l.is_empty())
                .map(|l| format!(" — {l}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "\n{}. {}{label}\n   {}\n   {}\n",
                i + 1,
                hit.title,
                citation(
                    &hit.doc_id,
                    hit.author.as_deref(),
                    &hit.published,
                    hit.tier.as_deref(),
                    &hit.url
                ),
                excerpt(&hit.snippet, RESULT_EXCERPT_CHARS + 50),
            ));
        }
        out.push_str(
            "\nNext: get_post_context(doc_id) for the full text (with thread context \
             for forum posts); get_topic(doc_id) for a whole forum discussion; \
             find_similar(doc_id) for related work.",
        );
        if self.embedder.is_none() {
            out.push_str(
                "\nnote: corpus has no vector index — ranking is lexical-only \
                 (run `corpus embed`).",
            );
        }
        Ok(out)
    }

    #[tool(
        name = "get_topic",
        description = "Fetch an entire discussion thread from the local corpus's forum sources (ethresear.ch, Ethereum Magicians): the original post in full, plus a one-line index of every reply (doc_id, author, date, opening words). Use this when a search_posts hit is a reply and you need the original post it responds to, or when you need the arc of the whole discussion — on these forums the objections, corrections, and author follow-ups in the replies routinely change the conclusions of the opening post. Pass the doc_id of ANY post in the thread (from search_posts results), or a numeric topic_id from a forum URL together with its source id (topic numbers collide across forums). Long threads are paged 50 replies at a time; pass reply_offset to continue. To read the full text of an interesting reply from the index, call get_post_context with that reply's doc_id. Forum threads only: EIPs, specs, and blog articles are standalone documents — get_post_context returns those in full."
    )]
    fn get_topic(&self, Parameters(p): Parameters<GetTopicParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let mut source_scope = p.source.clone();
        // A typo'd source must not masquerade as a missing topic.
        if let Some(src) = &source_scope {
            let stats = store.source_stats().map_err(internal)?;
            if !stats.iter().any(|s| &s.id == src) {
                return Err(ErrorData::invalid_params(
                    format!(
                        "unknown source {src:?} — known sources: {}",
                        stats.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                    None,
                ));
            }
        }
        // A doc_id, when present, always pins the source — models routinely
        // pass doc_id AND topic_id together, and the doc is the stronger
        // signal.
        let tid: i64 = if let Some(doc_id) = &p.doc_id {
            let doc = store
                .get(doc_id)
                .map_err(internal)?
                .ok_or_else(|| unknown_doc(doc_id))?;
            source_scope = Some(doc.source.clone());
            match (p.topic_id, doc.meta.get("topic_id").and_then(Value::as_i64)) {
                (Some(tid), _) => tid as i64,
                (None, Some(tid)) => tid,
                // Standalone document: nothing thread-shaped to fetch —
                // redirect instead of erroring (models call this on every
                // interesting hit).
                (None, None) => {
                    return Ok(format!(
                        "{doc_id:?} is a standalone document ({}) — an article or \
                         specification, not a forum thread, so there are no replies \
                         to fetch. Full text: get_post_context(\"{doc_id}\"). Related \
                         forum discussion: find_similar(\"{doc_id}\").",
                        doc.title,
                    ));
                }
            }
        } else if let Some(tid) = p.topic_id {
            tid as i64
        } else {
            return Err(ErrorData::invalid_params(
                "provide doc_id (from search_posts) or topic_id",
                None,
            ));
        };
        let mut posts = store
            .find_by_meta("topic_id", &Value::from(tid), source_scope.as_deref())
            .map_err(internal)?;
        if posts.is_empty() {
            return Err(ErrorData::invalid_params(
                format!("topic {tid} is not in the corpus"),
                None,
            ));
        }
        // Topic numbers collide across forums: an unscoped numeric id that
        // matches several sources is ambiguous, not mergeable.
        let mut hit_sources: Vec<&str> = posts.iter().map(|d| d.source.as_str()).collect();
        hit_sources.sort_unstable();
        hit_sources.dedup();
        if hit_sources.len() > 1 {
            return Err(ErrorData::invalid_params(
                format!(
                    "topic {tid} exists in multiple sources: {} — pass source, or the \
                     doc_id of a post in the thread you mean",
                    hit_sources.join(", ")
                ),
                None,
            ));
        }
        let source_id = hit_sources[0].to_string();
        let tier = store.source_tier(&source_id).map_err(internal)?;
        posts.sort_by_key(|d| {
            d.meta
                .get("post_number")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX)
        });

        let op = &posts[0];
        let first = posts.iter().map(|d| d.published.as_str()).min().unwrap_or("");
        let last = posts.iter().map(|d| d.published.as_str()).max().unwrap_or("");
        let mut out = format!(
            "Topic {source_id}/{tid}: {}\n{} posts · {} – {} · {}\n\n── Original post ──\n{}\n\n{}\n",
            op.title,
            posts.len(),
            date(first),
            date(last),
            op.url,
            citation(&op.id, op.author.as_deref(), &op.published, tier.as_deref(), &op.url),
            truncate_block(&op.content, OP_MAX_CHARS, &op.id),
        );

        let replies = &posts[1..];
        let offset = p.reply_offset.unwrap_or(0);
        if replies.is_empty() {
            out.push_str("\n(no replies)\n");
        } else if offset >= replies.len() {
            out.push_str(&format!(
                "\nreply_offset {offset} is beyond the last reply (the topic has {})\n",
                replies.len()
            ));
        } else {
            let end = (offset + REPLY_PAGE).min(replies.len());
            out.push_str(&format!(
                "\n── Replies {}–{} of {} ──\n",
                offset + 1,
                end,
                replies.len()
            ));
            for d in &replies[offset..end] {
                let pn = d
                    .meta
                    .get("post_number")
                    .and_then(Value::as_u64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into());
                out.push_str(&format!(
                    "#{pn} · {} · {} · {} · {}\n",
                    d.id,
                    d.author.as_deref().unwrap_or("unknown"),
                    date(&d.published),
                    excerpt(&d.content, INDEX_EXCERPT_CHARS),
                ));
            }
            if end < replies.len() {
                out.push_str(&format!(
                    "(more replies: call again with reply_offset={end})\n"
                ));
            }
        }
        out.push_str("\nFull text of any reply: get_post_context with its doc_id.");
        Ok(out)
    }

    #[tool(
        name = "get_post_context",
        description = "Fetch one document from the local corpus in full. Forum posts (ethresear.ch, Ethereum Magicians) come with their immediate conversation — a few thread posts before and after; standalone documents (EIP/ERC specifications, consensus specs, blog articles) come back whole. Use this whenever a search_posts or find_similar snippet looks relevant: replies usually only make sense next to what they answer, and the snippet alone is not enough to quote or cite responsibly. Takes a doc_id as returned by search_posts, get_topic, or find_similar. Every post in the output carries author, published date, source tier, and a citable URL — cite that URL when you use the content. For the whole thread rather than a local window, use get_topic instead."
    )]
    fn get_post_context(
        &self,
        Parameters(p): Parameters<GetPostContextParams>,
    ) -> Result<String, ErrorData> {
        let store = self.store();
        let doc = store
            .get(&p.doc_id)
            .map_err(internal)?
            .ok_or_else(|| unknown_doc(&p.doc_id))?;
        // Standalone documents (EIP/ERC specs, consensus specs, blog
        // articles) have no thread — their context is themselves. Ok text,
        // not an error: search_posts' footer sends models here for every
        // hit, and instructions recover better than protocol failures.
        let Some(tid) = doc.meta.get("topic_id").and_then(Value::as_i64) else {
            let tier = store.source_tier(&doc.source).map_err(internal)?;
            return Ok(format!(
                "Standalone document (not a forum thread) — full text follows.\n\n\
                 ── {} ──\n{}\n\n{}\n\n\
                 Related forum discussion: find_similar(\"{}\").",
                doc.title,
                citation(&doc.id, doc.author.as_deref(), &doc.published, tier.as_deref(), &doc.url),
                truncate_block(&doc.content, OP_MAX_CHARS, &doc.id),
                doc.id,
            ));
        };
        let target_pn = doc
            .meta
            .get("post_number")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());

        // Scoped to the anchor doc's source — topic ids collide across forums.
        let mut posts = store
            .find_by_meta("topic_id", &Value::from(tid), Some(&doc.source))
            .map_err(internal)?;
        let tier = store.source_tier(&doc.source).map_err(internal)?;
        posts.sort_by_key(|d| {
            d.meta
                .get("post_number")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX)
        });
        let pos = posts
            .iter()
            .position(|d| d.id == p.doc_id)
            .ok_or_else(|| internal(CoreError::Parse("post missing from own thread".into())))?;

        // Window by position, not post_number arithmetic — deleted posts
        // leave gaps in the numbering.
        let before = p.before.unwrap_or(2).min(MAX_CONTEXT);
        let after = p.after.unwrap_or(3).min(MAX_CONTEXT);
        let start = pos.saturating_sub(before);
        let end = (pos + after + 1).min(posts.len());

        let mut out = format!(
            "Thread: {} (topic {tid}, {} posts) — posts around #{target_pn}\n",
            doc.title,
            posts.len()
        );
        for (index, d) in posts[start..end].iter().enumerate() {
            let absolute = start + index;
            let pn = d
                .meta
                .get("post_number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let marker = if absolute == pos { "  ◀ requested post" } else { "" };
            let cap = if absolute == pos { OP_MAX_CHARS } else { NEIGHBOR_MAX_CHARS };
            out.push_str(&format!(
                "\n── #{pn} · {} ──{marker}\n{}\n",
                citation(&d.id, d.author.as_deref(), &d.published, tier.as_deref(), &d.url),
                truncate_block(&d.content, cap, &d.id),
            ));
        }
        out.push_str("\nMore: raise before/after, or get_topic for the full thread index.");
        Ok(out)
    }

    #[tool(
        name = "find_similar",
        description = "Find documents in the local corpus (research forums, EIP/ERC and consensus specs, blogs) that are semantically similar to a given one — nearest neighbors by embedding, not keyword overlap, including across sources. Use it to explore outward from a good hit: parallel proposals, competing mechanisms, the standards discussion of a research idea, and later posts revisiting the same design space share ideas but often not vocabulary, so keyword search misses them. Takes the doc_id of any document (from search_posts, get_topic, or get_post_context) and returns scored results with doc_id, author, published date, source tier, and citable URL. Comparing published dates across the results is the fastest way to trace how a line of research evolved and which design superseded which. Very short posts carry no embedding and return no neighbors — fall back to search_posts with the post's key phrases."
    )]
    fn find_similar(
        &self,
        Parameters(p): Parameters<FindSimilarParams>,
    ) -> Result<String, ErrorData> {
        let limit = p.limit.unwrap_or(10).clamp(1, MAX_LIMIT);
        let store = self.store();
        let doc = store
            .get(&p.doc_id)
            .map_err(internal)?
            .ok_or_else(|| unknown_doc(&p.doc_id))?;
        let Some(hits) = store.similar_docs(&p.doc_id, limit).map_err(internal)? else {
            // An expected state, not an error — the model recovers better
            // from instructions than from a protocol failure.
            return Ok(if store.embedding_count().map_err(internal)? == 0 {
                "The corpus has no vector index, so similarity search is unavailable — \
                 run `corpus embed` to build it. Meanwhile, search_posts still works."
                    .to_string()
            } else {
                format!(
                    "{:?} is too short to carry an embedding — try search_posts with \
                     its key phrases instead.",
                    p.doc_id
                )
            });
        };
        if hits.is_empty() {
            return Ok("No neighbors found.".to_string());
        }
        let mut out = format!(
            "Posts similar to {} — \"{}\" ({}, {}):\n",
            p.doc_id,
            doc.title,
            doc.author.as_deref().unwrap_or("unknown"),
            date(&doc.published),
        );
        for (i, hit) in hits.iter().enumerate() {
            let label = store
                .get(&hit.doc_id)
                .ok()
                .flatten()
                .map(|d| post_label(&d.meta))
                .filter(|l| !l.is_empty())
                .map(|l| format!(" — {l}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "\n{}. {}{label} · similarity {:.2}\n   {}\n   {}\n",
                i + 1,
                hit.title,
                hit.score,
                citation(
                    &hit.doc_id,
                    hit.author.as_deref(),
                    &hit.published,
                    hit.tier.as_deref(),
                    &hit.url
                ),
                excerpt(&hit.snippet, RESULT_EXCERPT_CHARS),
            ));
        }
        Ok(out)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CorpusServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(self.instructions.clone());
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corpus_core::Document;
    use serde_json::{Map, json};

    fn post(topic_id: u64, post_number: u64, content: &str) -> Document {
        let mut meta = Map::new();
        meta.insert("topic_id".into(), json!(topic_id));
        meta.insert("post_number".into(), json!(post_number));
        Document {
            id: format!("ethresearch/post/{topic_id}{post_number:03}"),
            source: "ethresearch".into(),
            url: format!("https://ethresear.ch/t/{topic_id}/{post_number}"),
            title: format!("Topic {topic_id}"),
            author: Some("tester".into()),
            published: "2024-05-01T00:00:00Z".into(),
            content: content.into(),
            meta,
        }
    }

    /// Topic 7 with post_numbers 1, 2, 5, 6 (3–4 deleted) and topic 8 with
    /// one post; no embedder.
    fn server() -> CorpusServer {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert(&[
                post(7, 1, "the original zorbling proposal, at length"),
                post(7, 2, "first reply about zorbling"),
                post(7, 5, "later reply, numbering gap before it"),
                post(7, 6, "final reply"),
                post(8, 1, "an unrelated flumph topic"),
            ])
            .unwrap();
        CorpusServer::new(store, None).unwrap()
    }

    /// A magicians post whose topic_id collides with ethresearch's topic 7.
    fn magicians_post(post_number: u64, content: &str) -> Document {
        let mut d = post(7, post_number, content);
        d.id = format!("ethmagicians/post/9{post_number:03}");
        d.source = "ethmagicians".into();
        d.url = format!("https://ethereum-magicians.org/t/7/{post_number}");
        d
    }

    fn colliding_server() -> CorpusServer {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert(&[
                post(7, 1, "the original zorbling proposal, at length"),
                post(7, 2, "first reply about zorbling"),
                magicians_post(1, "an unrelated magicians EIP discussion"),
                magicians_post(2, "magicians reply"),
            ])
            .unwrap();
        store
            .upsert_source("ethresearch", "https://ethresear.ch", "research")
            .unwrap();
        store
            .upsert_source("ethmagicians", "https://ethereum-magicians.org", "standards")
            .unwrap();
        CorpusServer::new(store, None).unwrap()
    }

    #[test]
    fn colliding_topic_ids_error_without_scope_and_resolve_with_it() {
        let s = colliding_server();
        // Bare numeric topic id across two forums: refuse to merge.
        let err = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(7),
                source: None,
                reply_offset: None,
            }))
            .unwrap_err();
        assert!(err.message.contains("ethmagicians"), "{}", err.message);
        assert!(err.message.contains("ethresearch"), "{}", err.message);

        // Scoped by source param: single forum.
        let out = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(7),
                source: Some("ethmagicians".into()),
                reply_offset: None,
            }))
            .unwrap();
        assert!(out.contains("Topic ethmagicians/7"));
        assert!(out.contains("magicians EIP discussion"));
        assert!(!out.contains("zorbling"));
        assert!(out.contains("standards"), "tier missing from citation");

        // Anchored by doc_id: the doc's source wins, no ambiguity.
        let out = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: Some("ethresearch/post/7002".into()),
                topic_id: None,
                source: None,
                reply_offset: None,
            }))
            .unwrap();
        assert!(out.contains("Topic ethresearch/7"));
        assert!(!out.contains("magicians"));

        // doc_id passed ALONGSIDE topic_id still pins the source — models
        // routinely send both.
        let out = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: Some("ethresearch/post/7002".into()),
                topic_id: Some(7),
                source: None,
                reply_offset: None,
            }))
            .unwrap();
        assert!(out.contains("Topic ethresearch/7"));

        // A typo'd source names the real ones instead of "not in corpus".
        let err = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(7),
                source: Some("ethereum-magicians".into()),
                reply_offset: None,
            }))
            .unwrap_err();
        assert!(err.message.contains("known sources"), "{}", err.message);
        assert!(err.message.contains("ethmagicians"), "{}", err.message);

        // get_post_context stays inside the anchor doc's forum too.
        let out = s
            .get_post_context(Parameters(GetPostContextParams {
                doc_id: "ethmagicians/post/9001".into(),
                before: None,
                after: None,
            }))
            .unwrap();
        assert!(out.contains("magicians"));
        assert!(!out.contains("zorbling"));
        assert!(out.contains("standards"), "tier missing from citation");
    }

    #[test]
    fn search_posts_output_carries_citations_and_footer() {
        let s = server();
        let out = s
            .search_posts(Parameters(SearchPostsParams {
                query: "zorbling".into(),
                limit: Some(2),
            }))
            .unwrap();
        assert!(out.contains("ethresearch/post/7001"));
        assert!(out.contains("https://ethresear.ch/t/7/1"));
        assert!(out.contains("2024-05-01"));
        assert!(out.contains("original post") || out.contains("reply #"));
        assert!(out.contains("lexical-only"), "no-embedder note missing");
        // limit honored: at most 2 numbered entries.
        assert!(!out.contains("\n3. "));
    }

    #[test]
    fn search_posts_empty_result_suggests_fallback() {
        let out = server()
            .search_posts(Parameters(SearchPostsParams {
                query: "wexlurb".into(),
                limit: None,
            }))
            .unwrap();
        assert!(out.contains("fall back to web search"));
    }

    #[test]
    fn get_topic_by_reply_doc_id_recovers_the_op() {
        let s = server();
        let out = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: Some("ethresearch/post/7005".into()),
                topic_id: None,
                source: None,
                reply_offset: None,
            }))
            .unwrap();
        assert!(out.contains("Original post"));
        assert!(out.contains("the original zorbling proposal"));
        assert!(out.contains("#2 · ethresearch/post/7002"));
        // Sorted despite the numbering gap.
        let p2 = out.find("#2 ·").unwrap();
        let p5 = out.find("#5 ·").unwrap();
        assert!(p2 < p5);
    }

    #[test]
    fn get_topic_by_topic_id_and_unknowns() {
        let s = server();
        assert!(
            s.get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(8),
                source: None,
                reply_offset: None,
            }))
            .unwrap()
            .contains("flumph")
        );
        assert!(
            s.get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(404),
                source: None,
                reply_offset: None,
            }))
            .is_err()
        );
        assert!(
            s.get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: None,
                source: None,
                reply_offset: None,
            }))
            .is_err()
        );
    }

    #[test]
    fn get_topic_pages_long_reply_lists() {
        let mut store = Store::open_in_memory().unwrap();
        let mut docs = vec![post(9, 1, "op")];
        for n in 2..=62 {
            docs.push(post(9, n, "reply"));
        }
        store.upsert(&docs).unwrap();
        let s = CorpusServer::new(store, None).unwrap();
        let page1 = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(9),
                source: None,
                reply_offset: None,
            }))
            .unwrap();
        assert!(page1.contains("Replies 1–50 of 61"));
        assert!(page1.contains("reply_offset=50"));
        let page2 = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: None,
                topic_id: Some(9),
                source: None,
                reply_offset: Some(50),
            }))
            .unwrap();
        assert!(page2.contains("Replies 51–61 of 61"));
        assert!(!page2.contains("more replies"));
    }

    #[test]
    fn get_post_context_windows_by_position_across_gaps() {
        let s = server();
        let out = s
            .get_post_context(Parameters(GetPostContextParams {
                doc_id: "ethresearch/post/7005".into(),
                before: Some(1),
                after: Some(1),
            }))
            .unwrap();
        // Positional neighbors of #5 are #2 and #6, not #4.
        assert!(out.contains("── #2 ·"));
        assert!(out.contains("── #5 ·"));
        assert!(out.contains("── #6 ·"));
        assert!(!out.contains("── #1 ·"));
        assert!(out.contains("◀ requested post"));
    }

    #[test]
    fn get_post_context_rejects_unknown_doc() {
        let err = server()
            .get_post_context(Parameters(GetPostContextParams {
                doc_id: "ethresearch/post/404404".into(),
                before: None,
                after: None,
            }))
            .unwrap_err();
        assert!(err.message.contains("not in the corpus"));
    }

    /// A standalone (non-thread) document — an EIP, spec, or blog article.
    fn standalone_doc() -> Document {
        Document {
            id: "eips/eip-9999".into(),
            source: "eips".into(),
            url: "https://eips.ethereum.org/EIPS/eip-9999".into(),
            title: "EIP-9999: Zorbling Precompile".into(),
            author: Some("someauthor".into()),
            published: "2026-01-01T00:00:00Z".into(),
            content: "The zorbling precompile enables efficient zorbling on L1.".into(),
            meta: Map::new(),
        }
    }

    #[test]
    fn get_post_context_returns_standalone_documents_whole() {
        let mut store = Store::open_in_memory().unwrap();
        store.upsert(&[standalone_doc()]).unwrap();
        store
            .upsert_source("eips", "https://github.com/ethereum/EIPs", "spec")
            .unwrap();
        let s = CorpusServer::new(store, None).unwrap();
        let out = s
            .get_post_context(Parameters(GetPostContextParams {
                doc_id: "eips/eip-9999".into(),
                before: None,
                after: None,
            }))
            .unwrap();
        assert!(out.contains("Standalone document"), "{out}");
        assert!(out.contains("zorbling precompile enables"), "full text present");
        assert!(out.contains("spec"), "tier in citation");
        assert!(out.contains("find_similar"), "redirect hint present");
    }

    #[test]
    fn get_topic_redirects_for_standalone_documents() {
        let mut store = Store::open_in_memory().unwrap();
        store.upsert(&[standalone_doc()]).unwrap();
        let s = CorpusServer::new(store, None).unwrap();
        let out = s
            .get_topic(Parameters(GetTopicParams {
                doc_id: Some("eips/eip-9999".into()),
                topic_id: None,
                source: None,
                reply_offset: None,
            }))
            .unwrap();
        assert!(out.contains("standalone document"), "{out}");
        assert!(out.contains("get_post_context"), "{out}");
    }

    #[test]
    fn find_similar_without_vector_space_is_friendly_text() {
        let out = server()
            .find_similar(Parameters(FindSimilarParams {
                doc_id: "ethresearch/post/7001".into(),
                limit: None,
            }))
            .unwrap();
        assert!(out.contains("corpus embed"));
    }
}
