//! The agent-level answer eval (ROADMAP M11): each question runs through a
//! headless Claude Code session with wikipethia as the ONLY tool source,
//! and the final answer is graded on whether it cites the expected
//! documents' URLs. This measures the whole loop — instructions, tool
//! descriptions, the model's query reformulation, retrieval, synthesis —
//! where `eval` measures the retrieval function alone.
//!
//! NOT a cargo test: every run consumes real usage — API credit or the
//! authenticated Claude plan's allowance, depending on how the `claude`
//! CLI is logged in — and needs network. The pure grading/parsing
//! functions below are unit-tested; the runner is exercised by running it.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use wikipethia_core::Store;
use serde_json::{Value, json};

use crate::eval::{parse_questions, resolve_expected_docs};

pub struct Config {
    pub db: PathBuf,
    pub questions: PathBuf,
    pub model: String,
    pub budget_per_question: f64,
    pub timeout_secs: u64,
    pub limit: Option<usize>,
    pub out_dir: Option<PathBuf>,
    /// The wikipethia-mcp binary the headless session will spawn.
    pub server_bin: PathBuf,
    /// Re-score an existing run's artifacts with the current grader — no
    /// sessions, no spend. The escape hatch for grader fixes.
    pub regrade: Option<PathBuf>,
}

/// Everything the built-in toolbox offers that could leak information in
/// from outside the corpus (or touch the machine). The run must answer
/// from wikipethia and reasoning alone — the shootout showed a capable
/// client silently routing around weak tool results via web search, which
/// would make this eval measure the client, not the server.
const DISALLOWED: &str = "Read,Write,Edit,Bash,Glob,Grep,WebSearch,WebFetch,Task,NotebookEdit";

pub fn run(config: &Config) -> anyhow::Result<()> {
    if let Some(dir) = &config.regrade {
        return regrade(dir);
    }
    let text = fs::read_to_string(&config.questions)
        .with_context(|| format!("reading {}", config.questions.display()))?;
    let mut questions = parse_questions(&text)?;
    if let Some(limit) = config.limit {
        questions.truncate(limit);
    }
    if questions.is_empty() {
        // --limit 0 would otherwise sail through to NaN means.
        bail!("no questions to run (is --limit 0?)");
    }

    let store = Store::open_existing(&config.db)?;
    let expected_urls: Vec<ExpectedUrls> = resolve_expected_docs(&store, &questions)?
        .into_iter()
        .map(|docs| ExpectedUrls {
            required: docs.required.into_iter().map(|d| d.url).collect(),
            alternatives: docs
                .alternatives
                .into_iter()
                .map(|group| group.into_iter().map(|d| d.url).collect())
                .collect(),
        })
        .collect();

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
    let (mcp_config, mut server_cmd) = write_mcp_config(&out_dir, &config.server_bin, &config.db)?;

    // One direct handshake with the exact configured server BEFORE any
    // paid session: a broken server otherwise burns a full budget per
    // question, for every question, answering from pretraining.
    probe_server(&mut server_cmd, Duration::from_secs(60))?;
    // …and one real session, because the handshake above proves only that
    // OUR binary works. See `probe_tools_reachable`.
    probe_tools_reachable(config, &mcp_config, &out_dir)?;

    let mut strict_total = 0.0;
    let mut thread_total = 0.0;
    let mut cost_total = 0.0;
    let mut failures = 0usize;
    let mut tool_calls_total = 0usize;
    println!("strict thread tools    cost  question");
    for (i, (q, urls)) in questions.iter().zip(&expected_urls).enumerate() {
        let run = run_one(config, &mcp_config, &out_dir, i, &q.question);
        if run.error.is_some() {
            failures += 1;
        }
        let answer = run.answer.as_deref().unwrap_or("");
        let (strict, thread) = grade(answer, urls);
        strict_total += strict;
        thread_total += thread;
        cost_total += run.cost_usd;
        tool_calls_total += run.queries.len();
        let flag = if run.error.is_some() { "  [FAILED]" } else { "" };
        println!(
            "  {strict:.2}   {thread:.2}  {:5}  ${:.2}  {}{flag}",
            run.queries.len(),
            run.cost_usd,
            q.question
        );
        let row = json!({
            "question": q.question,
            "expect": q.expect,
            "expect_any": q.expect_any,
            // Two keys, not one nested shape: `expected_urls` keeps the exact
            // meaning it had, so --regrade still reads pre-2026-08-19 runs
            // (baseline-m11, full-opus) and reproduces their numbers.
            "expected_urls": urls.required,
            "expected_url_groups": urls.alternatives,
            "strict": strict,
            "thread": thread,
            "cost_usd": run.cost_usd,
            "queries": run.queries,
            "answer": answer,
            "error": run.error,
        });
        fs::write(out_dir.join(format!("q{i:02}.json")), serde_json::to_string_pretty(&row)?)?;
    }

    let n = questions.len() as f64;
    // A sweep in which nothing ever called the corpus measured the model's
    // pretraining, not this server. The pre-sweep probe should have caught
    // it, so reaching here means the tools went away mid-run — either way the
    // means are not a baseline and must not be written down as one.
    let valid = tool_calls_total > 0;
    let summary = json!({
        "model": config.model,
        "questions": questions.len(),
        "failures": failures,
        "tool_calls": tool_calls_total,
        "valid": valid,
        "strict_mean": strict_total / n,
        "thread_mean": thread_total / n,
        // A killed session's spend has no result event to report it, so
        // this is a floor, not an invoice, whenever failures > 0.
        "total_cost_usd_lower_bound": cost_total,
    });
    fs::write(out_dir.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;
    let cost_note = if failures > 0 { " (lower bound — failed sessions still spend)" } else { "" };
    println!(
        "\nagent-eval: strict {:.3}, thread {:.3} over {} questions — {} failed, \
         {tool_calls_total} tool calls, ${cost_total:.2}{cost_note} total ({}), \
         artifacts in {}",
        strict_total / n,
        thread_total / n,
        questions.len(),
        failures,
        config.model,
        out_dir.display()
    );
    if !valid {
        bail!(
            "NOT A BASELINE: no question called the corpus even once, so these \
             means describe {}'s pretraining rather than this server. Do not \
             record them. (summary.json carries \"valid\": false.)",
            config.model
        );
    }
    Ok(())
}

/// Re-score persisted q*.json artifacts with the current grader. Free:
/// answers and expected URLs are already on disk.
fn regrade(dir: &Path) -> anyhow::Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('q') && n.ends_with(".json") && !n.contains('-'))
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        bail!("no q*.json artifacts in {}", dir.display());
    }
    let mut strict_total = 0.0;
    let mut thread_total = 0.0;
    println!("strict thread  question (regraded)");
    for path in &entries {
        let row: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
        let answer = row["answer"].as_str().unwrap_or("");
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        // `expected_url_groups` is absent from runs recorded before
        // 2026-08-19 and reads as empty, so those artifacts regrade to
        // exactly the numbers they were published with.
        let urls = ExpectedUrls {
            required: strings(&row["expected_urls"]),
            alternatives: row["expected_url_groups"]
                .as_array()
                .map(|groups| groups.iter().map(strings).collect())
                .unwrap_or_default(),
        };
        let (strict, thread) = grade(answer, &urls);
        strict_total += strict;
        thread_total += thread;
        println!("  {strict:.2}   {thread:.2}  {}", row["question"].as_str().unwrap_or("?"));
    }
    let n = entries.len() as f64;
    println!(
        "\nregraded: strict {:.3}, thread {:.3} over {} questions",
        strict_total / n,
        thread_total / n,
        entries.len()
    );
    Ok(())
}

struct RunOutcome {
    /// None when the session produced no result event.
    answer: Option<String>,
    queries: Vec<Value>,
    /// From the result event when one exists; a killed session's spend is
    /// real but unreportable, so 0.0 here means "unknown", not "free".
    cost_usd: f64,
    error: Option<String>,
}

/// One headless session. NOT `--bare`: bare mode restricts auth to
/// ANTHROPIC_API_KEY only, so OAuth-authenticated CLIs answer "Not logged
/// in" (measured). Isolation instead: `--strict-mcp-config` excludes every
/// MCP server but ours, and the child runs from the artifacts dir so no
/// project CLAUDE.md auto-discovers into the session (the user-level
/// ~/.claude context can still load — a documented, constant-across-runs
/// residue). `--max-budget-usd` bounds runaway agentic loops — this CLI
/// build has no --max-turns — and a watchdog kills the child at
/// timeout_secs. Never returns Err: every failure is embedded in the
/// outcome so the sweep continues and partial data (stream, cost,
/// queries) survives.
fn run_one(
    config: &Config,
    mcp_config: &Path,
    cwd: &Path,
    index: usize,
    question: &str,
) -> RunOutcome {
    match run_one_inner(config, mcp_config, cwd, index, question) {
        Ok(outcome) => outcome,
        Err(e) => RunOutcome {
            answer: None,
            queries: Vec::new(),
            cost_usd: 0.0,
            error: Some(format!("{e:#}")),
        },
    }
}

fn run_one_inner(
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
            // The question is a positional; the separator keeps a
            // contributed question starting with '-' from being parsed
            // as an option (measured: it errors the whole invocation).
            "--",
            question,
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
    let mut timed_out = false;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // The child is dead either way, so stdout is at EOF and the join is
    // prompt. The stream file is written on EVERY path — a hung run is
    // exactly when the audit trail matters.
    let lines = reader.join().expect("reader thread");
    fs::write(cwd.join(format!("q{index:02}-stream.jsonl")), lines.join("\n"))?;

    // Parse everything before judging anything: a bail that skips lines
    // discards the result event's cost sitting later in the same buffer.
    let mut answer = None;
    let mut cost = 0.0;
    let mut queries = Vec::new();
    let mut server_status = None;
    for line in &lines {
        match parse_stream_line(line) {
            Some(StreamEvent::ToolUse { name, input }) => {
                queries.push(json!({"tool": name, "input": input}));
            }
            Some(StreamEvent::Result { answer: a, cost_usd }) => {
                answer = Some(a);
                cost = cost_usd;
            }
            Some(StreamEvent::ServerStatus { status }) => server_status = Some(status),
            None => {}
        }
    }

    let error = if timed_out {
        Some(format!(
            "timed out after {}s — spend unreported; see q{index:02}-stream.jsonl",
            config.timeout_secs
        ))
    } else if server_status.as_deref().is_some_and(|s| s != "connected") {
        // The pre-sweep probe makes this a mid-run degradation signal
        // rather than the common path; the answer is pretraining, not
        // corpus, so it must not score.
        Some(format!(
            "wikipethia MCP server status {:?} mid-run — see {}",
            server_status.as_deref().unwrap_or("?"),
            stderr_path.display()
        ))
    } else if answer.is_none() {
        Some(format!(
            "no result event in {} stream lines — see {}",
            lines.len(),
            stderr_path.display()
        ))
    } else {
        None
    };
    Ok(RunOutcome {
        // A pretraining answer (server died mid-run) must not be graded.
        answer: if error.is_none() { answer } else { None },
        queries,
        cost_usd: cost,
        error,
    })
}

/// The --mcp-config payload: wikipethia and nothing else, absolute paths
/// so the headless session's cwd doesn't matter. Returns the config path
/// and the equivalent direct command for the pre-sweep probe.
fn write_mcp_config(
    out_dir: &Path,
    server_bin: &Path,
    db: &Path,
) -> anyhow::Result<(PathBuf, Command)> {
    let server_bin = server_bin.canonicalize().with_context(|| {
        format!(
            "wikipethia-mcp binary not found at {} — build it: cargo build --release -p wikipethia-mcp",
            server_bin.display()
        )
    })?;
    let db = db.canonicalize().context("db path")?;
    // fastembed's cache defaults to .fastembed_cache RELATIVE TO CWD, and
    // the headless session launches the server from the artifacts dir —
    // without an absolute cache path the server silently re-downloads the
    // model there (or hangs offline) and the MCP handshake times out. An
    // explicit FASTEMBED_CACHE_DIR wins; otherwise assume the invoker's
    // cwd, where `wikipethia embed` put the model.
    let cache = std::env::var("FASTEMBED_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join(".fastembed_cache"));
    let path = out_dir.join("mcp-config.json");
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
    let mut cmd = Command::new(&server_bin);
    cmd.args(["--db", &db.display().to_string()])
        .env("FASTEMBED_CACHE_DIR", &cache)
        .current_dir(out_dir);
    Ok((path, cmd))
}

/// One direct JSON-RPC initialize round-trip against the configured
/// server, exactly as the headless sessions will launch it.
/// Spend one session proving a headless client can actually **call** the
/// corpus tools, and abort the whole run if it cannot.
///
/// [`probe_server`] is not enough, and 2026-08-19 is how we know. Claude Code
/// 2.1.236 connects the MCP server, loads its instructions string, reports
/// `"status": "connected"` — and never registers its tool schemas. A headless
/// session then answers every question from pretraining. Nothing in the
/// stream says so: the status guard in `run_one_inner` sees `connected` and
/// passes, so a 33-question sweep would complete, report **`0 failed`**, and
/// record a confident 0.000 against a 0.693 baseline. That reads as a
/// catastrophic corpus regression caused by nothing at all, and it costs a
/// full sweep to produce.
///
/// A false abort costs one re-run; a false baseline gets written down. So
/// this probes on the configured model rather than a cheap tier — "the model
/// was too weak to call a tool" must not be confusable with "the tools are
/// not there" — and asks for a single call and nothing else, which keeps it
/// to a small fraction of one question's budget.
fn probe_tools_reachable(
    config: &Config,
    mcp_config: &Path,
    out_dir: &Path,
) -> anyhow::Result<()> {
    let probe = run_one(
        config,
        mcp_config,
        out_dir,
        // Its artifacts land beside the questions' as q99-*, out of the way
        // of the q00.. sequence --regrade reads.
        99,
        "Call the search_posts tool with the query \"proposer builder separation\". \
         Then reply with only the number of results. Do not answer from your own \
         knowledge and do not explain.",
    );
    if !probe.queries.is_empty() {
        return Ok(());
    }
    bail!(
        "the wikipethia tools are connected but not callable from a headless \
         session — a probe on {} made no tool call{}.\n\n\
         Every question would answer from pretraining and score ~0.00 while \
         reporting success, so this run is stopped before it spends anything \
         more. Known cause: Claude Code 2.1.236 does not register MCP tool \
         schemas in `-p` mode (the server still reports \"connected\", and \
         ToolSearch cannot find `mcp__wikipethia__*` either). Check `claude \
         --version`.\n\n\
         Probe artifacts: {}",
        config.model,
        probe
            .error
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default(),
        out_dir.join("q99-stream.jsonl").display(),
    )
}

fn probe_server(cmd: &mut Command, timeout: Duration) -> anyhow::Result<()> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning wikipethia-mcp for the pre-sweep probe")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\
              \"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\
              \"clientInfo\":{\"name\":\"agent-eval-probe\",\"version\":\"0\"}}}\n",
        )?;
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        BufReader::new(stdout).lines().next().and_then(Result::ok)
    });
    let deadline = Instant::now() + timeout;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Kill unconditionally: on success the probe is done with the server;
    // on deadline this EOFs stdout so the join below returns promptly.
    child.kill().ok();
    child.wait().ok();
    let first = reader.join().expect("probe reader");
    if !first.is_some_and(|l| l.contains("serverInfo")) {
        bail!(
            "the configured wikipethia server failed its initialize handshake — \
             fix this before paid sessions run (check the binary, --db, and the \
             fastembed cache)"
        );
    }
    Ok(())
}

enum StreamEvent {
    ToolUse { name: String, input: Value },
    Result { answer: String, cost_usd: f64 },
    /// The wikipethia entry of the init event's mcp_servers list.
    ServerStatus { status: String },
}

/// One line of `--output-format stream-json`. Only wikipethia tool calls,
/// the init event's server status, and the final result matter;
/// everything else (text deltas, tool results) is None.
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
/// Strict: the document's own URL is cited. Thread: for forum posts, any
/// URL from the same topic counts — a client that cites a thread's OP
/// after finding it via a reply has served the reader; this is the column
/// doc-id recall@10 structurally cannot measure (the M9 lesson).
/// One question's expected URLs, in the two scoring shapes.
///
/// `required` is all-of — every URL is wanted and counts separately.
/// `alternatives` is any-of — each group is one credit that any single
/// member earns, for questions where several sources answer equally well.
#[derive(Default)]
pub struct ExpectedUrls {
    pub required: Vec<String>,
    pub alternatives: Vec<Vec<String>>,
}

impl ExpectedUrls {
    fn credits(&self) -> usize {
        self.required.len() + self.alternatives.len()
    }
}

/// Citation recall of an answer, strict and thread-level.
///
/// Strict wants the expected document itself; thread also accepts any post
/// in the same thread, since a client that lands on a reply can recover the
/// original with `get_topic` and has demonstrably found the right discussion.
pub fn grade(answer: &str, expected: &ExpectedUrls) -> (f64, f64) {
    let credits = expected.credits();
    if credits == 0 {
        return (0.0, 0.0);
    }
    let cited = cited_urls(answer);
    let hit = |u: &String| cited.contains(u.trim_end_matches('/'));
    let hit_thread = |u: &String| {
        hit(u)
            || thread_prefix(u).is_some_and(|p| {
                cited.iter().any(|c| *c == p || c.starts_with(&format!("{p}/")))
            })
    };
    let score = |f: &dyn Fn(&String) -> bool| {
        let required = expected.required.iter().filter(|u| f(u)).count();
        // Any one member satisfies a group; citing three is not worth more
        // than citing one, because they are alternatives, not a checklist.
        let alternatives = expected
            .alternatives
            .iter()
            .filter(|group| group.iter().any(f))
            .count();
        (required + alternatives) as f64 / credits as f64
    };
    (score(&hit), score(&hit_thread))
}

/// Every URL cited in the answer, normalized (trailing slash and trailing
/// punctuation stripped). Extraction — not substring matching — because a
/// substring check has no right boundary: an expected OP URL ending in /1
/// is a prefix of its thread's replies /10–/19, which silently converted
/// thread-level citations into strict credit (9 of 15 expected docs end
/// in /1 today).
fn cited_urls(answer: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for (start, _) in answer.match_indices("http") {
        let rest = &answer[start..];
        if !rest.starts_with("http://") && !rest.starts_with("https://") {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || "()[]<>\"'`".contains(c))
            .unwrap_or(rest.len());
        let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', '*']);
        if !url.is_empty() {
            out.insert(url.trim_end_matches('/').to_string());
        }
    }
    out
}

/// The topic-level prefix of a Discourse post URL —
/// `https://host/t/slug/21517/34` → `https://host/t/slug/21517`, and the
/// slugless fallback shape ingest can emit (`/t/21517/34`) →
/// `https://host/t/21517`. None for anything not shaped like a forum post
/// (specs, blogs), where thread credit has no meaning beyond strict.
fn thread_prefix(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("/t/")?;
    let mut segments = rest.split('/');
    let first = segments.next()?;
    let host = &url[..url.len() - rest.len()];
    if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) {
        // Slugless: the first segment IS the topic id.
        return Some(format!("{host}{first}"));
    }
    let topic = segments.next()?;
    if topic.is_empty() || !topic.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{host}{first}/{topic}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required(urls: &[&str]) -> ExpectedUrls {
        ExpectedUrls {
            required: urls.iter().map(|s| s.to_string()).collect(),
            alternatives: Vec::new(),
        }
    }

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
        // The slugless fallback shape ingest can emit.
        assert_eq!(
            thread_prefix("https://ethresear.ch/t/21517/34").as_deref(),
            Some("https://ethresear.ch/t/21517")
        );
        assert_eq!(thread_prefix("https://eips.ethereum.org/EIPS/eip-4844"), None);
        assert_eq!(thread_prefix("https://ethresear.ch/t/slug-only"), None);
    }

    #[test]
    fn strict_credit_requires_the_exact_url_not_a_prefix() {
        // The bug the extraction rewrite fixes: expected OP /1 must NOT be
        // credited by a citation of reply /12.
        let expected = required(&["https://ethresear.ch/t/native-rollups/21517/1"]);
        let (strict, thread) =
            grade("See https://ethresear.ch/t/native-rollups/21517/12 here.", &expected);
        assert_eq!(strict, 0.0, "prefix must not earn strict credit");
        assert_eq!(thread, 1.0, "same thread still earns thread credit");
    }

    #[test]
    fn grading_gives_thread_credit_without_strict_credit() {
        let expected = required(&["https://ethresear.ch/t/native-rollups/21517/1"]);
        let answer = "See https://ethresear.ch/t/native-rollups/21517/34 for details.";
        assert_eq!(grade(answer, &expected), (0.0, 1.0));
        let answer = "See https://ethresear.ch/t/native-rollups/21517/1.";
        assert_eq!(grade(answer, &expected), (1.0, 1.0));
        assert_eq!(grade("no links here", &expected), (0.0, 0.0));
    }

    /// The measurement fix this field exists for: several sources answer
    /// "why does Ethereum have blobs?" equally well, and the opus run cited
    /// the EIP where `expect` named the forum thread — scoring 0.00 for a
    /// good answer. Any member of the group now earns the credit.
    #[test]
    fn an_alternatives_group_is_satisfied_by_any_member() {
        let expected = ExpectedUrls {
            required: Vec::new(),
            alternatives: vec![vec![
                "https://ethereum-magicians.org/t/eip-4844/8430/1".to_string(),
                "https://eips.ethereum.org/EIPS/eip-4844".to_string(),
            ]],
        };
        assert_eq!(grade("see https://eips.ethereum.org/EIPS/eip-4844", &expected), (1.0, 1.0));
        // Citing both is not worth more than citing one — they are
        // alternatives, not a checklist.
        assert_eq!(
            grade(
                "https://eips.ethereum.org/EIPS/eip-4844 and \
                 https://ethereum-magicians.org/t/eip-4844/8430/1",
                &expected
            ),
            (1.0, 1.0)
        );
        assert_eq!(grade("https://example.com/unrelated", &expected), (0.0, 0.0));
        // Thread credit still reaches a group member through its thread.
        let (strict, thread) =
            grade("https://ethereum-magicians.org/t/eip-4844/8430/17", &expected);
        assert_eq!((strict, thread), (0.0, 1.0));
    }

    /// Required and group credits share one denominator.
    #[test]
    fn required_and_alternatives_are_scored_together() {
        let expected = ExpectedUrls {
            required: vec!["https://a.io/x".to_string()],
            alternatives: vec![vec!["https://b.io/y".to_string(), "https://c.io/z".to_string()]],
        };
        assert_eq!(grade("https://a.io/x https://c.io/z", &expected), (1.0, 1.0));
        assert_eq!(grade("https://a.io/x", &expected).0, 0.5);
        assert_eq!(grade("https://b.io/y", &expected).0, 0.5);
    }

    #[test]
    fn url_extraction_handles_markdown_and_punctuation() {
        let cited = cited_urls(
            "Per [the EIP](https://eips.ethereum.org/EIPS/eip-4844), and \
             https://ethresear.ch/t/x/9/2. Also <https://a.io/b/>!",
        );
        assert!(cited.contains("https://eips.ethereum.org/EIPS/eip-4844"));
        assert!(cited.contains("https://ethresear.ch/t/x/9/2"));
        assert!(cited.contains("https://a.io/b"));
        // A bare "http" word is not a URL.
        assert!(cited_urls("http is a protocol").is_empty());
    }

    #[test]
    fn citation_matching_ignores_trailing_slash() {
        let (strict, _) = grade(
            "see https://eips.ethereum.org/EIPS/eip-4844/ here",
            &required(&["https://eips.ethereum.org/EIPS/eip-4844"]),
        );
        assert_eq!(strict, 1.0);
    }

    #[test]
    fn stream_lines_yield_wikipethia_calls_result_and_server_status() {
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
        let init = r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"wikipethia","status":"failed"}]}"#;
        match parse_stream_line(init) {
            Some(StreamEvent::ServerStatus { status }) => assert_eq!(status, "failed"),
            _ => panic!("server status not parsed"),
        }
        let other = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"WebSearch","input":{}}]}}"#;
        assert!(parse_stream_line(other).is_none());
        assert!(parse_stream_line("not json").is_none());
    }
}
