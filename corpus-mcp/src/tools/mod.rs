//! The corpus MCP tools. Per CLAUDE.md, every `description` string in here
//! is a prompt, not documentation — it is the only text a model reads when
//! deciding whether to reach for the corpus instead of a web search. Treat
//! edits to them as behavior changes.

pub mod format;

use std::sync::{Arc, Mutex, MutexGuard};

use corpus_core::{CoreError, Embedder, Store, spec};
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
    FUNCTION_MAX_CHARS, INDEX_EXCERPT_CHARS, MAX_CONTEXT, MAX_DEFINITIONS, MAX_LIMIT,
    NEIGHBOR_MAX_CHARS, OP_MAX_CHARS, REPLY_PAGE, RESULT_EXCERPT_CHARS, citation, date, excerpt,
    post_label, spec_status, truncate_block, window,
};

/// Clone is cheap by design — Arc bumps plus the instructions string. The
/// HTTP transport's handler factory clones one shared server per session
/// (and, for new-protocol clients, per request), so nothing expensive may
/// live directly in these fields.
#[derive(Clone)]
pub struct CorpusServer {
    /// rusqlite's Connection is Send but not Sync; tool bodies are sync
    /// `_impl` fns on spawn_blocking threads that hold the guard for one
    /// query — the guard never crosses an await. One shared connection
    /// serialized by this mutex, same semantics as stdio.
    store: Arc<Mutex<Store>>,
    /// None ⇒ the corpus has no vector index; ranking degrades to BM25.
    embedder: Option<Arc<FastEmbedder>>,
    /// Arc<str> so the per-request handler clones the HTTP transport
    /// performs are refcount bumps, not ~1.7KB heap copies.
    instructions: Arc<str>,
    tool_router: ToolRouter<Self>,
}

fn internal(e: CoreError) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn join_error(e: tokio::task::JoinError) -> ErrorData {
    ErrorData::internal_error(format!("blocking task failed: {e}"), None)
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
    /// Restrict results to documents whose id starts with this prefix: a
    /// source id ("eips", "ethresearch") or a deeper path
    /// ("consensusspecs/specs/electra" for one fork's spec documents).
    /// Omit to search the whole corpus.
    pub scope: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LookupSpecParams {
    /// The identifier exactly as the specs write it — a constant like
    /// "MAX_EFFECTIVE_BALANCE" or a spec function like "process_deposit".
    /// Case-sensitive; longer names containing this one also match
    /// (MAX_EFFECTIVE_BALANCE also surfaces MAX_EFFECTIVE_BALANCE_ELECTRA).
    pub name: String,
    /// Consensus fork to prefer, as a spec directory name: "phase0",
    /// "altair", "electra", "fulu", … Definitions from other forks and
    /// documents are still reported after the fork's own.
    pub fork: Option<String>,
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
    /// Character offset into the requested document, for paging through
    /// texts longer than one response (long EIP/spec documents routinely
    /// are). A truncated response says which offset to pass next.
    pub offset: Option<usize>,
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
             work; lookup_spec when the question names an exact spec identifier \
             (a constant's value or a spec function's body, optionally per fork). \
             Ethereum research supersedes \
             itself — always weigh published dates when posts disagree. One caution \
             for questions about the current or upcoming state of the protocol (the \
             next hardfork, what ships in it): do NOT trust discussion recency or \
             volume — fork planning pipelines overlap, so the fork after next always \
             has the newest and loudest threads. The reliable discriminator is the \
             \"Hardfork Meta\" EIPs: search for them and compare their status \
             fields to establish which fork is actually next. Relatedly, \
             forward-looking claims age with their document: an \"expected\" \
             or \"scheduled\" date is a projection as of the post's published \
             date. Check it against today's date, and report a lapsed \
             projection as a past expectation, not a current fact. Coverage is \
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
            store: Arc::new(Mutex::new(store)),
            embedder: embedder.map(Arc::new),
            instructions: instructions.into(),
            tool_router: Self::tool_router(),
        })
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        // A poisoned mutex means some tool body panicked mid-query. The
        // store is a read-only sqlite connection with no cross-call state
        // to corrupt, so recover and keep serving: under stdio a panic
        // killed one client's process, but the HTTP daemon must outlive
        // one bad query — a sticky panic here would brick every session
        // while the process stays alive, exactly where systemd's
        // Restart=on-failure cannot see it.
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[tool(
        name = "search_posts",
        description = "Search a local, curated corpus of Ethereum protocol research and standards: tens of thousands of posts from the ethresear.ch and Ethereum Magicians (ethereum-magicians.org) forums, 2017 to present, plus the full EIP and ERC specifications, the consensus-layer specs, the executable execution-layer spec (EELS, Python, per fork), the Engine API specs, AllCoreDevs meeting notes (what core devs decided and when), and articles from vitalik.eth.limo and blog.ethereum.org. Use this BEFORE web search for anything touching Ethereum research or the EIP process: sharding and danksharding, EIP-4844/blobs, account abstraction (EIP-4337/7702), proposer-builder separation (PBS), MEV, rollups, data availability sampling, statelessness, casper/consensus, staking economics, EIP and hard-fork coordination, or the cryptography behind them. Ranking is hybrid lexical+semantic, so exact tokens (\"EIP-4844\", an author's username) and natural-language questions both work. Every result carries a doc_id (the input to get_topic, get_post_context, and find_similar), author, published date, source tier, and a citable URL. Ethereum research goes stale in specific ways — a 2019 design post can be flatly superseded by a 2024 one — so always weigh the published dates when results disagree. But for questions about the current or upcoming state of the protocol (e.g. which hardfork is next), recency and thread volume mislead: fork planning pipelines overlap, so the fork after next has the newest, loudest discussion — resolve such questions from the status fields of the \"Hardfork Meta\" EIPs, not from what was posted most recently. Likewise, treat forward-looking dates in results (\"expected mid-2025\") as projections from that post's published date, not current facts — check them against today's date before repeating them. A top hit is often a reply from the middle of a thread: call get_post_context or get_topic with its doc_id to recover the original post and the surrounding argument; EIPs, specs, and blog articles are standalone documents, and get_post_context returns them whole. If nothing relevant returns, say so and fall back to web search rather than forcing a weak match."
    )]
    async fn search_posts(
        &self,
        Parameters(p): Parameters<SearchPostsParams>,
    ) -> Result<String, ErrorData> {
        // Tool bodies run SQL and (for search) ONNX inference while holding
        // the store guard — CPU-bound, blocking work that must not occupy
        // an async worker: under HTTP, concurrent sessions would starve the
        // runtime of workers for SSE keep-alives and shutdown.
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.search_posts_impl(p))
            .await
            .map_err(join_error)?
    }

    fn search_posts_impl(&self, p: SearchPostsParams) -> Result<String, ErrorData> {
        let limit = p.limit.unwrap_or(10).clamp(1, MAX_LIMIT);
        // scope: "" would silently behave as unscoped ("" prefixes every
        // id) — a blank almost always means an unset template variable.
        if p.scope.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err(ErrorData::invalid_params(
                "scope must be a source id or doc-id prefix — omit it entirely to \
                 search the whole corpus",
                None,
            ));
        }
        let query_vec = match &self.embedder {
            Some(embedder) => Some(embedder.embed_query(&p.query).map_err(internal)?),
            None => None,
        };
        let store = self.store();
        let hits = store
            .hybrid_search_scoped(&p.query, query_vec.as_deref(), p.scope.as_deref(), limit)
            .map_err(internal)?;
        if hits.is_empty() {
            // A scoped miss is usually a scope problem, not a coverage
            // problem — say so, or the client wrongly abandons the corpus.
            if let Some(scope) = &p.scope {
                let stats = store.source_stats().map_err(internal)?;
                let known: Vec<&str> = stats.iter().map(|s| s.id.as_str()).collect();
                let hint = if known.iter().any(|id| scope.starts_with(id)) {
                    "the scope matched no documents for this query — retry without \
                     scope before concluding the corpus lacks it"
                } else {
                    "the scope does not start with any indexed source id, so it can \
                     never match — retry without scope or fix it"
                };
                return Ok(format!(
                    "No results for {:?} within scope {scope:?}: {hint}. Indexed \
                     sources: {}.",
                    p.query,
                    known.join(", ")
                ));
            }
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
    async fn get_topic(
        &self,
        Parameters(p): Parameters<GetTopicParams>,
    ) -> Result<String, ErrorData> {
        // Tool bodies run SQL and (for search) ONNX inference while holding
        // the store guard — CPU-bound, blocking work that must not occupy
        // an async worker: under HTTP, concurrent sessions would starve the
        // runtime of workers for SSE keep-alives and shutdown.
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.get_topic_impl(p))
            .await
            .map_err(join_error)?
    }

    fn get_topic_impl(&self, p: GetTopicParams) -> Result<String, ErrorData> {
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
            // `window`, not truncate_block: get_post_context caps the OP at
            // the same OP_MAX_CHARS, so "call it for the full text" would
            // return this identical prefix — an 8k round trip that teaches
            // the model nothing. The offset hint is actionable immediately.
            window(&op.content, 0, OP_MAX_CHARS, &op.id),
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
        description = "Fetch one document from the local corpus. Forum posts (ethresear.ch, Ethereum Magicians) come with their immediate conversation — a few thread posts before and after; standalone documents (EIP/ERC specifications, consensus and execution-layer specs, Engine API specs, AllCoreDevs meeting notes, blog articles) come back on their own. Short documents arrive whole; longer ones page, and long EIPs and consensus specs usually do: a response ending in a truncation notice names the exact offset to pass on the next call, and sections near the end (Security Considerations, appendix tables, later constant tables) are ONLY reachable by following it — do not conclude a document lacks a section from one page. Use this whenever a search_posts or find_similar snippet looks relevant: replies usually only make sense next to what they answer, and the snippet alone is not enough to quote or cite responsibly. Takes a doc_id as returned by search_posts, get_topic, or find_similar. Every post in the output carries author, published date, source tier, and a citable URL — cite that URL when you use the content. For the whole thread rather than a local window, use get_topic instead."
    )]
    async fn get_post_context(
        &self,
        Parameters(p): Parameters<GetPostContextParams>,
    ) -> Result<String, ErrorData> {
        // Tool bodies run SQL and (for search) ONNX inference while holding
        // the store guard — CPU-bound, blocking work that must not occupy
        // an async worker: under HTTP, concurrent sessions would starve the
        // runtime of workers for SSE keep-alives and shutdown.
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.get_post_context_impl(p))
            .await
            .map_err(join_error)?
    }

    fn get_post_context_impl(&self, p: GetPostContextParams) -> Result<String, ErrorData> {
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
            // Spec status comes from EIP frontmatter, which ingest strips
            // out of `content` — without this line a model reading the full
            // text can never learn whether the spec is Draft or Final.
            let status = spec_status(&doc.meta)
                .map(|s| format!("\nStatus: {s}"))
                .unwrap_or_default();
            let body = window(&doc.content, p.offset.unwrap_or(0), OP_MAX_CHARS, &doc.id);
            // The header must not promise completeness a paged body does
            // not deliver: a model that anchors on "full text follows"
            // will report a section absent when it is merely on page 2.
            let complete = doc.content.chars().count() <= OP_MAX_CHARS;
            let header = if complete {
                "Standalone document (not a forum thread) — full text follows."
            } else {
                "Standalone document (not a forum thread) — too long for one \
                 response; this is one page of it (see the offset note below)."
            };
            return Ok(format!(
                "{header}\n\n── {} ──\n{}{status}\n\n{body}\n\n\
                 Related forum discussion: find_similar(\"{}\").",
                doc.title,
                citation(&doc.id, doc.author.as_deref(), &doc.published, tier.as_deref(), &doc.url),
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
        let offset = p.offset.unwrap_or(0);
        // Continuation pages carry the requested post alone: neighbors are
        // context for a first read, and re-sending them costs ~7.5k chars
        // per page while pushing the paging hint away from the tail, where
        // the tool description promises it.
        let paging = offset > 0;
        let before = if paging { 0 } else { p.before.unwrap_or(2).min(MAX_CONTEXT) };
        let after = if paging { 0 } else { p.after.unwrap_or(3).min(MAX_CONTEXT) };
        let start = pos.saturating_sub(before);
        let end = (pos + after + 1).min(posts.len());

        let mut out = if paging {
            format!("Thread: {} (topic {tid}) — post #{target_pn}, continued\n", doc.title)
        } else {
            format!(
                "Thread: {} (topic {tid}, {} posts) — posts around #{target_pn}\n",
                doc.title,
                posts.len()
            )
        };
        // Posts stay in thread order — a reply next to what it answers is
        // the reason this tool exists.
        let mut shown_end = 0usize;
        let mut truncated = false;
        for (index, d) in posts[start..end].iter().enumerate() {
            let absolute = start + index;
            let pn = d
                .meta
                .get("post_number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let is_requested = absolute == pos;
            let marker = if is_requested { "  ◀ requested post" } else { "" };
            // The requested post pages through `offset` (its truncation
            // hint must not point back at this same call); neighbors keep
            // the tighter cap and the truncate_block hint, which is
            // honest for them — requesting THAT doc really shows more.
            let body = if is_requested {
                let total = d.content.chars().count();
                shown_end = (offset + OP_MAX_CHARS).min(total);
                truncated = shown_end < total;
                window(&d.content, offset, OP_MAX_CHARS, &d.id)
            } else {
                truncate_block(&d.content, NEIGHBOR_MAX_CHARS, &d.id)
            };
            out.push_str(&format!(
                "\n── #{pn} · {} ──{marker}\n{body}\n",
                citation(&d.id, d.author.as_deref(), &d.published, tier.as_deref(), &d.url),
            ));
        }
        // Trailing neighbors would otherwise bury the requested post's
        // paging hint under THEIR hints, which name different doc_ids —
        // a model reading the tail to continue would follow the wrong one.
        if !paging {
            out.push_str("\nMore: raise before/after, or get_topic for the full thread index.");
        }
        if truncated {
            out.push_str(&format!(
                "\nPost #{target_pn} is truncated above: call get_post_context with \
                 doc_id={}, offset={shown_end} to continue reading it.",
                p.doc_id
            ));
        }
        Ok(out)
    }

    #[tool(
        name = "find_similar",
        description = "Find documents in the local corpus (research forums, EIP/ERC and consensus specs, blogs) that are semantically similar to a given one — nearest neighbors by embedding, not keyword overlap, including across sources. Use it to explore outward from a good hit: parallel proposals, competing mechanisms, the standards discussion of a research idea, and later posts revisiting the same design space share ideas but often not vocabulary, so keyword search misses them. Takes the doc_id of any document (from search_posts, get_topic, or get_post_context) and returns scored results with doc_id, author, published date, source tier, and citable URL. Comparing published dates across the results is the fastest way to trace how a line of research evolved and which design superseded which. Very short posts carry no embedding and return no neighbors — fall back to search_posts with the post's key phrases."
    )]
    async fn find_similar(
        &self,
        Parameters(p): Parameters<FindSimilarParams>,
    ) -> Result<String, ErrorData> {
        // Tool bodies run SQL and (for search) ONNX inference while holding
        // the store guard — CPU-bound, blocking work that must not occupy
        // an async worker: under HTTP, concurrent sessions would starve the
        // runtime of workers for SSE keep-alives and shutdown.
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.find_similar_impl(p))
            .await
            .map_err(join_error)?
    }

    fn find_similar_impl(&self, p: FindSimilarParams) -> Result<String, ErrorData> {
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

    #[tool(
        name = "lookup_spec",
        description = "Look up an exact identifier in the canonical Ethereum specifications held in the local corpus: a constant's value (MAX_EFFECTIVE_BALANCE, MIN_SLASHING_PENALTY_QUOTIENT) or a spec function's Python body (process_deposit, get_validator_churn_limit). Use this INSTEAD of search_posts whenever the question names a specific spec identifier — free-text search stems identifiers apart and ranks forum discussion above the defining document; this tool reads the spec documents themselves and returns every definition with a citable URL. Matching is case-sensitive and substring-based on the identifier, which matters: forks often introduce suffixed variants rather than redefining a name (MAX_EFFECTIVE_BALANCE_ELECTRA), so a base-name query intentionally returns the variants too — compare the fork labels in the citations to pick the right one, and don't assume the newest fork redefines every constant. Pass fork (a spec directory name — consensus-layer forks like \"electra\" or \"phase0\", execution-layer forks like \"cancun\", \"prague\", \"osaka\") to put that fork's definitions first, which is usually essential for execution-layer identifiers: the executable spec keeps one near-identical copy per fork, so an unfiltered lookup returns two dozen variants of the same function; other forks' definitions still follow, because the value a fork actually uses is often inherited from an earlier one or lives under a suffixed name. Returns nothing for concepts or prose — for those use search_posts."
    )]
    async fn lookup_spec(
        &self,
        Parameters(p): Parameters<LookupSpecParams>,
    ) -> Result<String, ErrorData> {
        // Tool bodies run SQL and (for search) ONNX inference while holding
        // the store guard — CPU-bound, blocking work that must not occupy
        // an async worker: under HTTP, concurrent sessions would starve the
        // runtime of workers for SSE keep-alives and shutdown.
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.lookup_spec_impl(p))
            .await
            .map_err(join_error)?
    }

    fn lookup_spec_impl(&self, p: LookupSpecParams) -> Result<String, ErrorData> {
        let name = p.name.trim();
        if name.is_empty() {
            return Err(ErrorData::invalid_params("name must be a spec identifier", None));
        }
        // A blank fork is almost always an unset template variable; failing
        // loudly beats emitting the misleading "not in fork \"\"" preamble.
        if p.fork.as_deref().is_some_and(|f| f.trim().is_empty()) {
            return Err(ErrorData::invalid_params(
                "fork must be a spec directory name like \"electra\" — omit it entirely \
                 to search all forks",
                None,
            ));
        }
        let store = self.store();
        // Spec-tier sources by manifest tier — never a hardcoded source list.
        let stats = store.source_stats().map_err(internal)?;
        let spec_sources: Vec<String> = stats
            .iter()
            .filter(|s| s.tier.as_deref() == Some("spec"))
            .map(|s| s.id.clone())
            .collect();
        if spec_sources.is_empty() {
            return Ok("No spec-tier sources are indexed in this corpus — lookup_spec \
                       has nothing to read. Use search_posts instead."
                .to_string());
        }
        let docs = store.docs_containing(name, &spec_sources).map_err(internal)?;

        // (in_fork, kind_is_function, exact, doc index, rendered block)
        let fork_needle = p.fork.as_deref().map(|f| format!("/{f}/"));
        let mut blocks: Vec<(bool, bool, bool, String)> = Vec::new();
        for doc in &docs {
            // Every doc came from a tier="spec" source by construction —
            // no per-doc tier query needed.
            let cite = citation(&doc.id, doc.author.as_deref(), &doc.published, Some("spec"), &doc.url);
            // An EIP/ERC definition's authority hinges on its status
            // (Withdrawn vs Final) — the same field the temporal-trap
            // labels exist for; a lookup result must not hide it.
            let status = spec_status(&doc.meta)
                .map(|s| format!(" [status: {s}]"))
                .unwrap_or_default();
            let in_fork = fork_needle.as_deref().is_some_and(|f| doc.id.contains(f));
            for c in spec::constants(&doc.content) {
                if !c.name.contains(name) {
                    continue;
                }
                let desc = c.description.map(|d| format!(" — {d}")).unwrap_or_default();
                blocks.push((
                    in_fork,
                    false,
                    c.name == name,
                    format!("{} = {}{desc}{status}\n   {cite}", c.name, c.value),
                ));
            }
            // A .py document IS Python; everything else is prose that may
            // quote Python inside fences.
            let functions = if doc.id.ends_with(".py") {
                spec::functions_in_python(&doc.content)
            } else {
                spec::functions(&doc.content)
            };
            for f in functions {
                if !f.name.contains(name) {
                    continue;
                }
                let label = f.heading.map(|h| format!(" [{h}]")).unwrap_or_default();
                // A long body's continuation hint must land the model ON
                // the function, not on character 0 of a 78k-char spec —
                // locate the `def` line and hand get_post_context its
                // offset. Byte→char conversion because offsets are chars.
                let code = if f.code.chars().count() <= FUNCTION_MAX_CHARS {
                    f.code.clone()
                } else {
                    let head: String = f.code.chars().take(FUNCTION_MAX_CHARS).collect();
                    let at = f
                        .code
                        .lines()
                        .next()
                        .and_then(|def| doc.content.find(def))
                        .map(|byte| doc.content[..byte].chars().count());
                    match at {
                        Some(at) => format!(
                            "{}\n… [function truncated — call get_post_context with \
                             doc_id={}, offset={at} to read it in the spec]",
                            head.trim_end(),
                            doc.id
                        ),
                        None => format!("{}\n… [function truncated]", head.trim_end()),
                    }
                };
                blocks.push((
                    in_fork,
                    true,
                    f.name == name,
                    format!("{}{label}{status}\n```python\n{code}\n```\n   {cite}", f.name),
                ));
            }
        }

        if blocks.is_empty() {
            return Ok(format!(
                "No spec definition matches {name:?}. Identifiers are case-sensitive \
                 and matched verbatim (constants LIKE_THIS, functions like_this) — \
                 check the spelling, or use search_posts for concept-level questions."
            ));
        }

        // Fork-preferred first, then exact-name before variants, constants
        // before functions; stable within groups (docs arrive id-ordered).
        blocks.sort_by_key(|(in_fork, is_fn, exact, _)| (!in_fork, *is_fn, !exact));
        let total = blocks.len();
        let shown = total.min(MAX_DEFINITIONS);
        let mut out = match (&p.fork, blocks.first()) {
            (Some(fork), Some((false, ..))) => format!(
                "No definition matching {name:?} inside fork {fork:?} — fork \
                 directories only carry what they change, so the governing \
                 definition is usually inherited or suffixed. Definitions found \
                 elsewhere:\n"
            ),
            (Some(fork), _) => format!(
                "Definitions matching {name:?} — fork {fork:?} first, then other \
                 documents (inherited/suffixed definitions often govern):\n"
            ),
            (None, _) => format!("Definitions matching {name:?} across the spec corpus:\n"),
        };
        for (_, _, _, block) in blocks.iter().take(shown) {
            out.push('\n');
            out.push_str(block);
            out.push('\n');
        }
        if shown < total {
            out.push_str(&format!(
                "\n({} more definitions matched — pass `fork` to select one fork's \
                 copy, or narrow the name.)",
                total - shown
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
        info.instructions = Some(self.instructions.to_string());
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
            .get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(7),
                source: None,
                reply_offset: None,
            })
            .unwrap_err();
        assert!(err.message.contains("ethmagicians"), "{}", err.message);
        assert!(err.message.contains("ethresearch"), "{}", err.message);

        // Scoped by source param: single forum.
        let out = s
            .get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(7),
                source: Some("ethmagicians".into()),
                reply_offset: None,
            })
            .unwrap();
        assert!(out.contains("Topic ethmagicians/7"));
        assert!(out.contains("magicians EIP discussion"));
        assert!(!out.contains("zorbling"));
        assert!(out.contains("standards"), "tier missing from citation");

        // Anchored by doc_id: the doc's source wins, no ambiguity.
        let out = s
            .get_topic_impl(GetTopicParams {
                doc_id: Some("ethresearch/post/7002".into()),
                topic_id: None,
                source: None,
                reply_offset: None,
            })
            .unwrap();
        assert!(out.contains("Topic ethresearch/7"));
        assert!(!out.contains("magicians"));

        // doc_id passed ALONGSIDE topic_id still pins the source — models
        // routinely send both.
        let out = s
            .get_topic_impl(GetTopicParams {
                doc_id: Some("ethresearch/post/7002".into()),
                topic_id: Some(7),
                source: None,
                reply_offset: None,
            })
            .unwrap();
        assert!(out.contains("Topic ethresearch/7"));

        // A typo'd source names the real ones instead of "not in corpus".
        let err = s
            .get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(7),
                source: Some("ethereum-magicians".into()),
                reply_offset: None,
            })
            .unwrap_err();
        assert!(err.message.contains("known sources"), "{}", err.message);
        assert!(err.message.contains("ethmagicians"), "{}", err.message);

        // get_post_context stays inside the anchor doc's forum too.
        let out = s
            .get_post_context_impl(GetPostContextParams {
                doc_id: "ethmagicians/post/9001".into(),
                before: None,
                after: None,
                offset: None,
            })
            .unwrap();
        assert!(out.contains("magicians"));
        assert!(!out.contains("zorbling"));
        assert!(out.contains("standards"), "tier missing from citation");
    }

    /// Two forks of a spec doc plus an EIP, all tier "spec", and one forum
    /// post that mentions the constant but must never surface in lookups.
    fn spec_server() -> CorpusServer {
        let phase0 = "# Phase0 -- The Beacon Chain\n\n\
            | Name | Value |\n| - | - |\n\
            | `MAX_WIDGET_BALANCE` | `Gwei(2**5 * 10**9)` (= 32,000,000,000) |\n\n\
            ###### `process_widget`\n\n\
            ```python\ndef process_widget(state: BeaconState) -> None:\n    pass\n```\n";
        let electra = "# Electra -- The Beacon Chain\n\n\
            | Name | Value | Description |\n| - | - | - |\n\
            | `MAX_WIDGET_BALANCE_ELECTRA` | `Gwei(2**11 * 10**9)` | Compounding widgets |\n\n\
            ###### Modified `process_widget`\n\n\
            ```python\ndef process_widget(state: BeaconState) -> None:\n    return None\n```\n";
        let mut store = Store::open_in_memory().unwrap();
        let doc = |id: &str, source: &str, content: &str| Document {
            id: id.into(),
            source: source.into(),
            url: format!("https://example.com/{id}"),
            title: "spec doc".into(),
            author: None,
            published: "2026-01-01T00:00:00Z".into(),
            content: content.into(),
            meta: Map::new(),
        };
        let mut eip = doc(
            "eips/eip-9999",
            "eips",
            "| Name | Value |\n| - | - |\n| `MAX_WIDGET_BALANCE_ELECTRA` | `Gwei(2**11 * 10**9)` (2048 ETH) |",
        );
        eip.meta.insert("status".into(), json!("Withdrawn"));
        store
            .upsert(&[
                doc("specs/specs/phase0/beacon-chain", "specs", phase0),
                doc("specs/specs/electra/beacon-chain", "specs", electra),
                eip,
                doc("ethresearch/post/1", "ethresearch", "forum chatter on MAX_WIDGET_BALANCE"),
            ])
            .unwrap();
        store.upsert_source("specs", "https://example.com/specs", "spec").unwrap();
        store.upsert_source("eips", "https://example.com/eips", "spec").unwrap();
        store
            .upsert_source("ethresearch", "https://ethresear.ch", "research")
            .unwrap();
        CorpusServer::new(store, None).unwrap()
    }

    #[test]
    fn lookup_spec_returns_fork_first_with_variants_and_citations() {
        let s = spec_server();
        let out = s
            .lookup_spec_impl(LookupSpecParams {
                name: "MAX_WIDGET_BALANCE".into(),
                fork: Some("electra".into()),
            })
            .unwrap();
        // Electra's suffixed variant leads; phase0's base value follows.
        let electra_pos = out.find("MAX_WIDGET_BALANCE_ELECTRA").expect("variant present");
        let phase0_pos = out.find("(= 32,000,000,000)").expect("base value present");
        assert!(electra_pos < phase0_pos, "fork-preferred ordering:\n{out}");
        assert!(out.contains("Compounding widgets"), "description kept");
        assert!(out.contains("https://example.com/specs/specs/phase0/beacon-chain"));
        // Research-tier mentions never masquerade as definitions.
        assert!(!out.contains("ethresear.ch"), "forum leaked into spec lookup:\n{out}");
    }

    #[test]
    fn lookup_spec_finds_functions_with_their_spec_labels() {
        let s = spec_server();
        let out = s
            .lookup_spec_impl(LookupSpecParams {
                name: "process_widget".into(),
                fork: None,
            })
            .unwrap();
        assert!(out.contains("```python"), "{out}");
        assert!(out.contains("def process_widget"));
        assert!(out.contains("Modified `process_widget`"), "spec label kept:\n{out}");
    }

    #[test]
    fn lookup_spec_miss_is_instructional_not_an_error() {
        let s = spec_server();
        let out = s
            .lookup_spec_impl(LookupSpecParams {
                name: "NO_SUCH_NAME".into(),
                fork: None,
            })
            .unwrap();
        assert!(out.contains("case-sensitive"), "{out}");
        // A fork with no in-fork match still reports elsewhere-definitions.
        let out = s
            .lookup_spec_impl(LookupSpecParams {
                name: "MAX_WIDGET_BALANCE".into(),
                fork: Some("fulu".into()),
            })
            .unwrap();
        assert!(out.contains("No definition matching"), "{out}");
        assert!(out.contains("inherited"), "explains fork inheritance:\n{out}");
        assert!(out.contains("MAX_WIDGET_BALANCE"), "still shows definitions");
    }

    #[test]
    fn lookup_spec_labels_definitions_with_spec_status() {
        let s = spec_server();
        let out = s
            .lookup_spec_impl(LookupSpecParams {
                name: "MAX_WIDGET_BALANCE".into(),
                fork: None,
            })
            .unwrap();
        // The Withdrawn EIP's definition must not read like a Final one.
        assert!(out.contains("[status: Withdrawn]"), "{out}");
        // Consensus-spec docs carry no status meta — no empty label.
        assert!(!out.contains("[status: ]"), "{out}");
    }

    #[test]
    fn blank_fork_and_scope_are_loud_errors() {
        let s = spec_server();
        assert!(s
            .lookup_spec_impl(LookupSpecParams {
                name: "MAX_WIDGET_BALANCE".into(),
                fork: Some("  ".into()),
            })
            .is_err());
        assert!(s
            .search_posts_impl(SearchPostsParams {
                query: "widget".into(),
                limit: None,
                scope: Some("".into()),
            })
            .is_err());
    }

    #[test]
    fn scoped_empty_results_explain_the_scope() {
        let s = spec_server();
        // Valid scope, no matches: advise retrying unscoped.
        let out = s
            .search_posts_impl(SearchPostsParams {
                query: "flumph".into(),
                limit: None,
                scope: Some("eips".into()),
            })
            .unwrap();
        assert!(out.contains("within scope"), "{out}");
        assert!(out.contains("retry without"), "{out}");
        // A scope matching no indexed source id: say it can never match.
        let out = s
            .search_posts_impl(SearchPostsParams {
                query: "widget".into(),
                limit: None,
                scope: Some("consensus-specs".into()),
            })
            .unwrap();
        assert!(out.contains("never match"), "{out}");
        assert!(out.contains("ethresearch"), "lists indexed sources: {out}");
    }

    #[test]
    fn long_standalone_documents_page_through_offset() {
        let mut store = Store::open_in_memory().unwrap();
        let long = format!("HEAD-MARKER {} TAIL-MARKER", "filler word ".repeat(1_000));
        store
            .upsert(&[Document {
                id: "eips/eip-9998".into(),
                source: "eips".into(),
                url: "https://example.com/eip-9998".into(),
                title: "Long spec".into(),
                author: None,
                published: "2026-01-01T00:00:00Z".into(),
                content: long.clone(),
                meta: Map::new(),
            }])
            .unwrap();
        let s = CorpusServer::new(store, None).unwrap();
        let page1 = s
            .get_post_context_impl(GetPostContextParams {
                doc_id: "eips/eip-9998".into(),
                before: None,
                after: None,
                offset: None,
            })
            .unwrap();
        assert!(page1.contains("HEAD-MARKER"));
        assert!(!page1.contains("TAIL-MARKER"), "12k chars must not fit one page");
        // The hint must NOT be the circular "call get_post_context for the
        // full text" — it names the next offset.
        // Slice by chars, not bytes: the output carries …, –, and ── , so
        // a byte slice would panic instead of printing this diagnostic.
        let tail: String = page1.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        assert!(!page1.contains("for the full text"), "{tail}");
        let next = page1
            .split("offset=")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .and_then(|s| s.parse::<usize>().ok())
            .expect("continuation offset in hint");
        let page2 = s
            .get_post_context_impl(GetPostContextParams {
                doc_id: "eips/eip-9998".into(),
                before: None,
                after: None,
                offset: Some(next),
            })
            .unwrap();
        let head: String = page2.chars().take(200).collect();
        assert!(page2.contains("continuing from character"), "{head}");
        assert!(page2.contains("TAIL-MARKER"), "second page reaches the tail");
    }

    #[test]
    fn the_header_only_promises_full_text_when_it_delivers_it() {
        let mut store = Store::open_in_memory().unwrap();
        let doc = |id: &str, content: String| Document {
            id: id.into(),
            source: "eips".into(),
            url: format!("https://example.com/{id}"),
            title: "Spec".into(),
            author: None,
            published: "2026-01-01T00:00:00Z".into(),
            content,
            meta: Map::new(),
        };
        store
            .upsert(&[
                doc("eips/eip-1000", "short and complete".into()),
                doc("eips/eip-1001", "filler word ".repeat(1_000)),
            ])
            .unwrap();
        let s = CorpusServer::new(store, None).unwrap();
        let params = |id: &str| GetPostContextParams {
            doc_id: id.into(),
            before: None,
            after: None,
            offset: None,
        };
        let short = s.get_post_context_impl(params("eips/eip-1000")).unwrap();
        assert!(short.contains("full text follows"), "{short}");
        let long = s.get_post_context_impl(params("eips/eip-1001")).unwrap();
        assert!(!long.contains("full text follows"), "header must not promise completeness");
        assert!(long.contains("one page of it"), "{}", long.chars().take(200).collect::<String>());
    }

    #[test]
    fn a_long_thread_post_gets_a_tail_pointer_to_its_own_continuation() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert(&[
                post(7, 1, "the original zorbling proposal"),
                post(7, 2, &format!("HEAD {} TAIL", "long reply text ".repeat(700))),
                post(7, 3, "a short following reply"),
            ])
            .unwrap();
        let s = CorpusServer::new(store, None).unwrap();
        let page1 = s
            .get_post_context_impl(GetPostContextParams {
                doc_id: "ethresearch/post/7002".into(),
                before: None,
                after: None,
                offset: None,
            })
            .unwrap();
        // Neighbors are present on page 1, and the LAST thing in the
        // response points at the requested post, not at a neighbor.
        assert!(page1.contains("ethresearch/post/7003"), "neighbors on page 1");
        let tail: String = page1.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
        assert!(tail.contains("doc_id=ethresearch/post/7002"), "tail points at requested: {tail}");
        let next: usize = tail
            .split("offset=")
            .nth(1)
            .and_then(|s| s.trim_end_matches(" to continue reading it.").parse().ok())
            .expect("offset in tail pointer");
        // Page 2 is the requested post alone — no neighbor re-send.
        let page2 = s
            .get_post_context_impl(GetPostContextParams {
                doc_id: "ethresearch/post/7002".into(),
                before: None,
                after: None,
                offset: Some(next),
            })
            .unwrap();
        assert!(page2.contains("continued"), "{}", page2.chars().take(120).collect::<String>());
        assert!(page2.contains("TAIL"), "second page reaches the end");
        assert!(!page2.contains("ethresearch/post/7003"), "neighbors suppressed while paging");
    }

    #[test]
    fn search_posts_scope_narrows_results() {
        let s = spec_server();
        let out = s
            .search_posts_impl(SearchPostsParams {
                query: "MAX_WIDGET_BALANCE".into(),
                limit: None,
                scope: Some("ethresearch".into()),
            })
            .unwrap();
        assert!(out.contains("ethresearch/post/1"), "{out}");
        assert!(!out.contains("eips/eip-9999"), "scope leaked:\n{out}");
    }

    #[tokio::test]
    async fn async_tool_wrappers_round_trip_via_spawn_blocking() {
        let s = server();
        let out = s
            .search_posts(Parameters(SearchPostsParams {
                query: "zorbling".into(),
                limit: Some(1),
                scope: None,
            }))
            .await
            .unwrap();
        assert!(out.contains("results for"), "{out}");
    }

    #[test]
    fn search_posts_output_carries_citations_and_footer() {
        let s = server();
        let out = s
            .search_posts_impl(SearchPostsParams {
                query: "zorbling".into(),
                limit: Some(2),
                scope: None,
            })
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
            .search_posts_impl(SearchPostsParams {
                query: "wexlurb".into(),
                limit: None,
                scope: None,
            })
            .unwrap();
        assert!(out.contains("fall back to web search"));
    }

    #[test]
    fn get_topic_by_reply_doc_id_recovers_the_op() {
        let s = server();
        let out = s
            .get_topic_impl(GetTopicParams {
                doc_id: Some("ethresearch/post/7005".into()),
                topic_id: None,
                source: None,
                reply_offset: None,
            })
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
            s.get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(8),
                source: None,
                reply_offset: None,
            })
            .unwrap()
            .contains("flumph")
        );
        assert!(
            s.get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(404),
                source: None,
                reply_offset: None,
            })
            .is_err()
        );
        assert!(
            s.get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: None,
                source: None,
                reply_offset: None,
            })
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
            .get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(9),
                source: None,
                reply_offset: None,
            })
            .unwrap();
        assert!(page1.contains("Replies 1–50 of 61"));
        assert!(page1.contains("reply_offset=50"));
        let page2 = s
            .get_topic_impl(GetTopicParams {
                doc_id: None,
                topic_id: Some(9),
                source: None,
                reply_offset: Some(50),
            })
            .unwrap();
        assert!(page2.contains("Replies 51–61 of 61"));
        assert!(!page2.contains("more replies"));
    }

    #[test]
    fn get_post_context_windows_by_position_across_gaps() {
        let s = server();
        let out = s
            .get_post_context_impl(GetPostContextParams {
                doc_id: "ethresearch/post/7005".into(),
                before: Some(1),
                after: Some(1),
                offset: None,
            })
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
            .get_post_context_impl(GetPostContextParams {
                doc_id: "ethresearch/post/404404".into(),
                before: None,
                after: None,
                offset: None,
            })
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
            .get_post_context_impl(GetPostContextParams {
                doc_id: "eips/eip-9999".into(),
                before: None,
                after: None,
                offset: None,
            })
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
            .get_topic_impl(GetTopicParams {
                doc_id: Some("eips/eip-9999".into()),
                topic_id: None,
                source: None,
                reply_offset: None,
            })
            .unwrap();
        assert!(out.contains("standalone document"), "{out}");
        assert!(out.contains("get_post_context"), "{out}");
    }

    #[test]
    fn find_similar_without_vector_space_is_friendly_text() {
        let out = server()
            .find_similar_impl(FindSimilarParams {
                doc_id: "ethresearch/post/7001".into(),
                limit: None,
            })
            .unwrap();
        assert!(out.contains("corpus embed"));
    }
}
