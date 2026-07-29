//! HTTP client, rate limiting, and fetch adapters. All network access lives here.

pub mod client;
pub mod discourse;
pub mod error;
pub mod sync;

pub use client::{Clock, HttpClient, RealClock};
pub use error::FetchError;
pub use sync::{Fetcher, SyncOptions, SyncStats, sync, sync_topic};
