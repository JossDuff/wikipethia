//! The agent-level answer eval (ROADMAP M11): each question runs through a
//! headless Claude Code session with wikipethia as the ONLY tool source,
//! and the final answer is graded on whether it cites the expected
//! documents' URLs. This measures the whole loop — instructions, tool
//! descriptions, the model's query reformulation, retrieval, synthesis —
//! where `eval` measures the retrieval function alone.
//!
//! NOT a cargo test: every run costs real API money and needs network.
//! The pure grading/parsing functions below are unit-tested; the runner
//! is exercised by running it.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use corpus_core::Store;
use serde_json::{Value, json};

use crate::eval::parse_questions;

pub struct Config {
    pub db: PathBuf,
    pub questions: PathBuf,
    pub model: String,
    pub budget_per_question: f64,
    pub timeout_secs: u64,
    pub limit: Option<usize>,
    pub out_dir: Option<PathBuf>,
    /// The corpus-mcp binary the headless session will spawn.
    pub server_bin: PathBuf,
}

/// Everything the built-in toolbox offers that could leak information in
/// from outside the corpus (or touch the machine). The run must answer
/// from wikipethia and reasoning alone — the shootout showed a capable
/// client silently routing around weak tool results via web search, which
/// would make this eval measure the client, not the server.
const DISALLOWED: &str = "Read,Write,Edit,Bash,Glob,Grep,WebSearch,WebFetch,Task,NotebookEdit";

pub fn run(config: &Config) -> anyhow::Result<()> {
    let text = fs::read_to_string(&config.questions)
        .with_context(|| format!("reading {}", config.questions.display()))?;
    let mut questions = parse_questions(&text)?;
    if let Some(limit) = config.limit {
        questions.truncate(limit);
    }

    // Resolve expected doc ids to URLs up front — an unknown id is a typo
    // in the questions file, same preflight contract as `eval`.
    let store = Store::open(&config.db)?;
    let expected_urls: Vec<Vec<String>> = questions
        .iter()
        .map(|q| {
            q.expect
                .iter()
                .map(|id| {
                    store
                        .get(id)?
                        .map(|d| d.url)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "expect id {id:?} (question {:?}) is not in the corpus",
                                q.question
                            )
                        })
                })
                .collect()
        })
        .collect::<anyhow::Result<_>>()?;

    let out_dir = match &config.out_dir {
        Some(dir) => dir.clone(),
        None => {
            let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            PathBuf::from(format!("eval-runs/{epoch}"))
        }
    };
    fs::create_dir_all(&out_dir)?;
    // Absolute from here on: the child runs with its cwd set to this
    // directory and resolves relative paths against it, not against ours.
    let out_dir = out_dir.canonicalize()?;
    let mcp_config = write_mcp_config(&out_dir, &config.server_bin, &config.db)?;

    let mut rows: Vec<Value> = Vec::new();
    let (mut strict_total, mut thread_total, mut cost_total) = (0.0, 0.0, 0.0);
    let mut failures = 0usize;
    println!("strict thread tools    cost  question");
    for (i, (q, urls)) in questions.iter().zip(&expected_urls).enumerate() {
        let outcome = run_one(config, &mcp_config, &out_dir, i, &q.question);
        let (answer, queries, cost, error) = match outcome {
            Ok(run) => (run.answer, run.queries, run.cost_usd, None),
            Err(e) => {
                failures += 1;
                (String::new(), Vec::new(), 0.0, Some(format!("{e:#}")))
            }
        };
        let (strict, thread) = grade(&answer, urls);
        strict_total += strict;
        thread_total += thread;
        cost_total += cost;
        let flag = if error.is_some() { "  [FAILED]" } else { "" };
        println!(
            "  {strict:.2}   {thread:.2}  {:5}  ${cost:.2}  {}{flag}",
            queries.len(),
            q.question
        );
        let row = json!({
            "question": q.question,
            "expect": q.expect,
            "expected_urls": urls,
            "strict": strict,
            "thread": thread,
            "cost_usd": cost,
            "queries": queries,
            "answer": answer,
            "error": error,
        });
        fs::write(out_dir.join(format!("q{i:02}.json")), serde_json::to_string_pretty(&row)?)?;
        rows.push(row);
    }

    let n = questions.len() as f64;
    let summary = json!({
        "model": config.model,
        "questions": questions.len(),
        "failures": failures,
        "strict_mean": strict_total / n,
        "thread_mean": thread_total / n,
        "total_cost_usd": cost_total,
    });
    fs::write(out_dir.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;
    println!(
        "\nagent-eval: strict {:.3}, thread {:.3} over {} questions — {} failed, \
         ${cost_total:.2} total ({}), artifacts in {}",
        strict_total / n,
        thread_total / n,
        questions.len(),
        failures,
        config.model,
        out_dir.display()
    );
    Ok(())
}

struct RunOutcome {
    answer: String,
    queries: Vec<Value>,
    cost_usd: f64,
}

/// One headless session. NOT `--bare`: bare mode restricts auth to
/// ANTHROPIC_API_KEY only, so OAuth-authenticated CLIs answer "Not logged
/// in" (measured). Isolation instead: `--strict-mcp-config` excludes every
/// MCP server but ours, and the child runs from the artifacts dir so no
/// project CLAUDE.md auto-discovers into the session (the user-level
/// ~/.claude context can still load — a documented, constant-across-runs
/// residue). `--max-budget-usd` bounds runaway agentic loops — this CLI
/// build has no --max-turns — and a watchdog kills the child at
/// timeout_secs.
fn run_one(
    config: &Config,
    mcp_config: &Path,
    cwd: &Path,
    index: usize,
    question: &str,
) -> anyhow::Result<RunOutcome> {
    // Child stderr goes to a file, never /dev/null — "0 stream lines"
    // failures are undiagnosable without it.
    let stderr_path = cwd.join(format!("q{index:02}-stderr.log"));
    let stderr_file = fs::File::create(&stderr_path)?;
    let mut child = Command::new("claude")
        .current_dir(cwd)
        .args([
            "--strict-mcp-config",
            "-p",
            question,
            "--mcp-config",
            &mcp_config.display().to_string(),
            "--allowedTools",
            "mcp__wikipethia__*",
            "--disallowedTools",
            DISALLOWED,
            "--permission-mode",
            "dontAsk",
            "--model",
            &config.model,
            "--max-budget-usd",
            &config.budget_per_question.to_string(),
            "--output-format",
            "stream-json",
            "--verbose",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .stdin(Stdio::null())
        .spawn()
        .context("spawning `claude` — is Claude Code installed and on PATH?")?;

    // Reader thread drains stdout so the child never blocks on a full
    // pipe; the main thread owns the deadline.
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        BufReader::new(stdout).lines().map_while(Result::ok).collect::<Vec<String>>()
    });
    let deadline = Instant::now() + Duration::from_secs(config.timeout_secs);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            bail!("timed out after {}s", config.timeout_secs);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let lines = reader.join().expect("reader thread");
    // The raw stream is the audit trail — which tools really ran, whether
    // anything non-wikipethia leaked in — and the only way to debug a
    // malformed run after the fact.
    fs::write(cwd.join(format!("q{index:02}-stream.jsonl")), lines.join("\n"))?;

    let mut answer = None;
    let mut cost = 0.0;
    let mut queries = Vec::new();
    for line in &lines {
        match parse_stream_line(line) {
            Some(StreamEvent::ToolUse { name, input }) => {
                queries.push(json!({"tool": name, "input": input}));
            }
            Some(StreamEvent::Result { answer: a, cost_usd }) => {
                answer = Some(a);
                cost = cost_usd;
            }
            Some(StreamEvent::ServerStatus { status }) if status != "connected" => {
                // Without the server the model answers from pretraining
                // and the score measures nothing — fail loudly instead.
                bail!(
                    "wikipethia MCP server status {status:?} — see {} and the \
                     mcp-config",
                    stderr_path.display()
                );
            }
            _ => {}
        }
    }
    let answer = answer.ok_or_else(|| {
        anyhow::anyhow!(
            "no result event in {} stream lines — see {}",
            lines.len(),
            stderr_path.display()
        )
    })?;
    Ok(RunOutcome {
        answer,
        queries,
        cost_usd: cost,
    })
}

/// The --mcp-config payload: wikipethia and nothing else, absolute paths
/// so the headless session's cwd doesn't matter.
fn write_mcp_config(out_dir: &Path, server_bin: &Path, db: &Path) -> anyhow::Result<PathBuf> {
    let server_bin = server_bin.canonicalize().with_context(|| {
        format!(
            "corpus-mcp binary not found at {} — build it: cargo build --release -p corpus-mcp",
            server_bin.display()
        )
    })?;
    let db = db.canonicalize().context("db path")?;
    let path = out_dir.join("mcp-config.json");
    // fastembed's cache defaults to .fastembed_cache RELATIVE TO CWD, and
    // the headless session launches the server from the artifacts dir —
    // without an absolute cache path the server silently re-downloads the
    // model there (or hangs offline) and the MCP handshake times out.
    // Point it at the invoker's cache, where `corpus embed` put the model.
    let cache = std::env::current_dir()?.join(".fastembed_cache");
    let config = json!({
        "mcpServers": {
            "wikipethia": {
                "type": "stdio",
                "command": server_bin.display().to_string(),
                "args": ["--db", db.display().to_string()],
                "env": { "FASTEMBED_CACHE_DIR": cache.display().to_string() },
            }
        }
    });
    fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    Ok(path)
}

enum StreamEvent {
    ToolUse { name: String, input: Value },
    Result { answer: String, cost_usd: f64 },
    /// The wikipethia entry of the init event's mcp_servers list.
    ServerStatus { status: String },
}

/// One line of `--output-format stream-json`. Only wikipethia tool calls
/// and the final result matter; everything else (init, text deltas, tool
/// results) is None.
fn parse_stream_line(line: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v.get("type")?.as_str()? {
        "assistant" => {
            let blocks = v.get("message")?.get("content")?.as_array()?;
            for block in blocks {
                if block.get("type")?.as_str()? == "tool_use" {
                    let name = block.get("name")?.as_str()?;
                    if name.starts_with("mcp__wikipethia__") {
                        return Some(StreamEvent::ToolUse {
                            name: name.to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        });
                    }
                }
            }
            None
        }
        "result" => Some(StreamEvent::Result {
            answer: v.get("result")?.as_str()?.to_string(),
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        }),
        "system" => {
            if v.get("subtype")?.as_str()? != "init" {
                return None;
            }
            let servers = v.get("mcp_servers")?.as_array()?;
            let wiki = servers
                .iter()
                .find(|s| s.get("name").and_then(Value::as_str) == Some("wikipethia"))?;
            Some(StreamEvent::ServerStatus {
                status: wiki.get("status")?.as_str()?.to_string(),
            })
        }
        _ => None,
    }
}

/// Fractions of expected URLs the answer credits, (strict, thread).
/// Strict: the document's own URL appears. Thread: for forum posts, any
/// URL from the same topic counts — a client that cites a thread's OP
/// after finding it via a reply has served the reader; this is the column
/// doc-id recall@10 structurally cannot measure (the M9 lesson).
pub fn grade(answer: &str, expected_urls: &[String]) -> (f64, f64) {
    if expected_urls.is_empty() {
        return (0.0, 0.0);
    }
    let strict = expected_urls.iter().filter(|u| answer_cites(answer, u)).count();
    let thread = expected_urls
        .iter()
        .filter(|u| {
            answer_cites(answer, u)
                || thread_prefix(u).is_some_and(|p| answer_cites(answer, &p))
        })
        .count();
    let n = expected_urls.len() as f64;
    (strict as f64 / n, thread as f64 / n)
}

/// Trailing-slash-insensitive substring: citations routinely drop or add
/// the final slash.
fn answer_cites(answer: &str, url: &str) -> bool {
    answer.contains(url.trim_end_matches('/'))
}

/// The topic-level prefix of a Discourse post URL —
/// `https://host/t/slug/21517/34` → `https://host/t/slug/21517` — and
/// None for anything not shaped like a forum post (specs, blogs), where
/// thread credit has no meaning beyond strict.
fn thread_prefix(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("/t/")?;
    let mut segments = rest.split('/');
    let slug = segments.next()?;
    let topic = segments.next()?;
    if topic.is_empty() || !topic.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let host = &url[..url.len() - rest.len()];
    Some(format!("{host}{slug}/{topic}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_prefixes_only_for_forum_post_urls() {
        assert_eq!(
            thread_prefix("https://ethresear.ch/t/native-rollups/21517/34").as_deref(),
            Some("https://ethresear.ch/t/native-rollups/21517")
        );
        // Topic-level URL is its own prefix.
        assert_eq!(
            thread_prefix("https://ethresear.ch/t/native-rollups/21517").as_deref(),
            Some("https://ethresear.ch/t/native-rollups/21517")
        );
        assert_eq!(thread_prefix("https://eips.ethereum.org/EIPS/eip-4844"), None);
        assert_eq!(thread_prefix("https://ethresear.ch/t/slug-only"), None);
    }

    #[test]
    fn grading_gives_thread_credit_without_strict_credit() {
        let expected = vec!["https://ethresear.ch/t/native-rollups/21517/1".to_string()];
        // Cites a DIFFERENT post of the same thread.
        let answer = "See https://ethresear.ch/t/native-rollups/21517/34 for details.";
        assert_eq!(grade(answer, &expected), (0.0, 1.0));
        // Cites the exact post: both.
        let answer = "See https://ethresear.ch/t/native-rollups/21517/1.";
        assert_eq!(grade(answer, &expected), (1.0, 1.0));
        // Cites nothing relevant.
        assert_eq!(grade("no links here", &expected), (0.0, 0.0));
    }

    #[test]
    fn citation_matching_ignores_trailing_slash() {
        assert!(answer_cites("x https://eips.ethereum.org/EIPS/eip-4844 y",
            "https://eips.ethereum.org/EIPS/eip-4844/"));
        let (strict, _) = grade(
            "https://eips.ethereum.org/EIPS/eip-4844",
            &["https://eips.ethereum.org/EIPS/eip-4844".to_string()],
        );
        assert_eq!(strict, 1.0);
    }

    #[test]
    fn stream_lines_yield_wikipethia_calls_and_result() {
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__wikipethia__search_posts","input":{"query":"PBS"}}]}}"#;
        match parse_stream_line(tool) {
            Some(StreamEvent::ToolUse { name, input }) => {
                assert_eq!(name, "mcp__wikipethia__search_posts");
                assert_eq!(input["query"], "PBS");
            }
            _ => panic!("tool_use not parsed"),
        }
        let result = r#"{"type":"result","result":"the answer","total_cost_usd":0.12}"#;
        match parse_stream_line(result) {
            Some(StreamEvent::Result { answer, cost_usd }) => {
                assert_eq!(answer, "the answer");
                assert!((cost_usd - 0.12).abs() < 1e-9);
            }
            _ => panic!("result not parsed"),
        }
        // Non-wikipethia tools and other event types are ignored.
        let other = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"WebSearch","input":{}}]}}"#;
        assert!(parse_stream_line(other).is_none());
        assert!(parse_stream_line(r#"{"type":"system","subtype":"init"}"#).is_none());
        assert!(parse_stream_line("not json").is_none());
    }
}
