//! HTTP client, rate limiting, and fetch adapters. All network access lives here.

pub mod adapter;
pub mod client;
pub mod discourse;
pub mod error;
pub mod feed;
pub mod html;
pub mod repo;
pub mod sync;
mod xml;

pub use adapter::{Adapter, DiscourseAdapter};
pub use client::{Clock, HttpClient, RealClock};
pub use error::FetchError;
pub use feed::FeedAdapter;
pub use repo::{RepoAdapter, TRACK_DEFAULT};
pub use sync::{Fetcher, SyncOptions, SyncStats, sync, sync_topic};
