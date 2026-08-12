//! HTTP client with per-client politeness: one request per second, backoff
//! on 429 (honoring `Retry-After`) and on transient failures.
//!
//! The limiter is per `HttpClient` instance, not per host. "One request per
//! second per host" (the CLAUDE.md rule) holds because the CLI creates one
//! client per source and runs sources sequentially — never point two live
//! clients at the same host.

use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::Agent;

use crate::error::FetchError;

const USER_AGENT: &str = "wikipethia/0.1 (personal research corpus; jduff360@gmail.com)";
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Topic payloads with `include_raw=1` on long threads run to a few MB.
const BODY_LIMIT: u64 = 64 * 1024 * 1024;

/// Time source, injectable so tests never sleep for real.
pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&mut self, dur: Duration);
}

pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, dur: Duration) {
        std::thread::sleep(dur);
    }
}

/// Outcome of a single request attempt, before retry policy is applied.
/// Generic over the body type so JSON, text, and byte fetches share one
/// throttle/backoff/429 policy.
enum Attempt<T> {
    Success(T),
    Retry {
        retry_after: Option<Duration>,
        reason: String,
    },
    Fatal(FetchError),
}

pub struct HttpClient<C: Clock = RealClock> {
    agent: Agent,
    clock: C,
    last_request: Option<Instant>,
}

impl HttpClient<RealClock> {
    pub fn new() -> Self {
        Self::with_clock(RealClock)
    }
}

impl Default for HttpClient<RealClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> HttpClient<C> {
    pub fn with_clock(clock: C) -> Self {
        let config = Agent::config_builder()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        Self {
            agent: config.into(),
            clock,
            last_request: None,
        }
    }

    /// GET `url` and parse the body as JSON, throttled and with retries.
    pub fn get_json(&mut self, url: &str) -> Result<Value, FetchError> {
        let agent = self.agent.clone();
        self.run_with_retries(url, move |u| {
            Self::attempt(&agent, u, |body| {
                body.with_config()
                    .limit(BODY_LIMIT)
                    .read_json::<Value>()
            })
        })
    }

    /// GET `url` as text (RSS/atom XML, HTML pages), throttled and retried.
    pub fn get_text(&mut self, url: &str) -> Result<String, FetchError> {
        let agent = self.agent.clone();
        self.run_with_retries(url, move |u| {
            Self::attempt(&agent, u, |body| {
                body.with_config()
                    .limit(BODY_LIMIT)
                    .read_to_string()
            })
        })
    }

    /// GET `url` as raw bytes (repo tarballs), throttled and retried.
    /// Bodies are bounded by [`BODY_LIMIT`]; snapshot tarballs run 10–30 MB.
    pub fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, FetchError> {
        let agent = self.agent.clone();
        self.run_with_retries(url, move |u| {
            Self::attempt(&agent, u, |body| {
                body.with_config()
                    .limit(BODY_LIMIT)
                    .read_to_vec()
            })
        })
    }

    /// Sleep however long is needed so consecutive requests are at least
    /// `MIN_INTERVAL` apart.
    fn throttle(&mut self) {
        let now = self.clock.now();
        if let Some(last) = self.last_request {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < MIN_INTERVAL {
                self.clock.sleep(MIN_INTERVAL - elapsed);
            }
        }
        self.last_request = Some(self.clock.now());
    }

    fn run_with_retries<T>(
        &mut self,
        url: &str,
        mut attempt_fn: impl FnMut(&str) -> Attempt<T>,
    ) -> Result<T, FetchError> {
        let mut backoff = INITIAL_BACKOFF;
        let mut last_error = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            self.throttle();
            match attempt_fn(url) {
                Attempt::Success(value) => return Ok(value),
                Attempt::Fatal(err) => return Err(err),
                Attempt::Retry {
                    retry_after,
                    reason,
                } => {
                    last_error = reason;
                    if attempt == MAX_ATTEMPTS {
                        break;
                    }
                    let delay = retry_after.map_or(backoff, |ra| ra.max(backoff));
                    // Unlogged waits look exactly like a hang from outside —
                    // on rate-limited hosts these can chain to minutes.
                    eprintln!(
                        "warn: retry {attempt}/{MAX_ATTEMPTS} for {url}: {last_error} — \
                         waiting {}s",
                        delay.as_secs()
                    );
                    self.clock.sleep(delay);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        Err(FetchError::RetriesExhausted {
            url: url.to_string(),
            attempts: MAX_ATTEMPTS,
            last_error,
        })
    }

    fn attempt<T>(
        agent: &Agent,
        url: &str,
        read_body: impl Fn(&mut ureq::Body) -> Result<T, ureq::Error>,
    ) -> Attempt<T> {
        let mut response = match agent.get(url).call() {
            Ok(response) => response,
            Err(err) => {
                return Attempt::Retry {
                    retry_after: None,
                    reason: format!("transport error: {err}"),
                };
            }
        };
        let status = response.status();
        if status.is_success() {
            match read_body(response.body_mut()) {
                Ok(value) => Attempt::Success(value),
                // Over the limit is deterministic — retrying would download
                // the same 64 MiB five times just to fail with a misleading
                // "truncation" message.
                Err(err @ ureq::Error::BodyExceedsLimit(_)) => {
                    Attempt::Fatal(FetchError::Shape(format!(
                        "{url}: response body over the {BODY_LIMIT}-byte limit ({err})"
                    )))
                }
                // Any other bad body on a 200 is most likely truncation; retry.
                Err(err) => Attempt::Retry {
                    retry_after: None,
                    reason: format!("bad body: {err}"),
                },
            }
        } else if status.as_u16() == 429 {
            Attempt::Retry {
                retry_after: retry_after(&response),
                reason: "HTTP 429".to_string(),
            }
        } else if status.is_server_error() {
            Attempt::Retry {
                retry_after: None,
                reason: format!("HTTP {status}"),
            }
        } else {
            Attempt::Fatal(FetchError::Status {
                url: url.to_string(),
                status: status.as_u16(),
            })
        }
    }
}

/// Parse a `Retry-After: <seconds>` header. The HTTP-date form is ignored.
fn retry_after<B>(response: &ureq::http::Response<B>) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// Clock that advances only when slept on, recording every sleep.
    struct FakeClock {
        start: Instant,
        offset: Duration,
        sleeps: Rc<RefCell<Vec<Duration>>>,
    }

    impl FakeClock {
        fn new() -> (Self, Rc<RefCell<Vec<Duration>>>) {
            let sleeps = Rc::new(RefCell::new(Vec::new()));
            let clock = Self {
                start: Instant::now(),
                offset: Duration::ZERO,
                sleeps: Rc::clone(&sleeps),
            };
            (clock, sleeps)
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.start + self.offset
        }

        fn sleep(&mut self, dur: Duration) {
            self.offset += dur;
            self.sleeps.borrow_mut().push(dur);
        }
    }

    fn success() -> Attempt<Value> {
        Attempt::Success(Value::Null)
    }

    #[test]
    fn second_request_waits_out_the_remainder_of_one_second() {
        let (clock, sleeps) = FakeClock::new();
        let mut client = HttpClient::with_clock(clock);

        client.run_with_retries("u1", |_| success()).unwrap();
        assert!(sleeps.borrow().is_empty(), "first request must not wait");

        client.run_with_retries("u2", |_| success()).unwrap();
        assert_eq!(*sleeps.borrow(), vec![MIN_INTERVAL]);
    }

    #[test]
    fn retry_after_header_stretches_the_backoff() {
        let (clock, sleeps) = FakeClock::new();
        let mut client = HttpClient::with_clock(clock);

        let mut calls = 0;
        client
            .run_with_retries("u", |_| {
                calls += 1;
                if calls == 1 {
                    Attempt::Retry {
                        retry_after: Some(Duration::from_secs(30)),
                        reason: "HTTP 429".into(),
                    }
                } else {
                    success()
                }
            })
            .unwrap();

        assert_eq!(calls, 2);
        // The 30 s Retry-After wins over the 1 s initial backoff. No throttle
        // sleep before the retry: 30 s have already passed.
        assert_eq!(*sleeps.borrow(), vec![Duration::from_secs(30)]);
    }

    #[test]
    fn backoff_doubles_and_retries_are_capped() {
        let (clock, sleeps) = FakeClock::new();
        let mut client = HttpClient::with_clock(clock);

        let mut calls = 0;
        let err = client
            .run_with_retries("u", |_| -> Attempt<Value> {
                calls += 1;
                Attempt::Retry {
                    retry_after: None,
                    reason: "HTTP 503".into(),
                }
            })
            .unwrap_err();

        assert_eq!(calls, MAX_ATTEMPTS);
        assert!(matches!(err, FetchError::RetriesExhausted { attempts, .. } if attempts == MAX_ATTEMPTS));
        // Four backoff sleeps between five attempts: 1, 2, 4, 8 seconds.
        let backoffs: Vec<_> = sleeps
            .borrow()
            .iter()
            .copied()
            .filter(|d| *d >= INITIAL_BACKOFF)
            .collect();
        assert_eq!(
            backoffs,
            [1, 2, 4, 8].map(Duration::from_secs).to_vec()
        );
    }

    #[test]
    fn fatal_status_does_not_retry() {
        let (clock, _sleeps) = FakeClock::new();
        let mut client = HttpClient::with_clock(clock);

        let mut calls = 0;
        let err = client
            .run_with_retries("u", |_| -> Attempt<Value> {
                calls += 1;
                Attempt::Fatal(FetchError::Status {
                    url: "u".into(),
                    status: 404,
                })
            })
            .unwrap_err();

        assert_eq!(calls, 1);
        assert!(matches!(err, FetchError::Status { status: 404, .. }));
    }
}
