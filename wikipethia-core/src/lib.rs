//! Documents, adapters, index, and search. No I/O beyond the database.

pub mod chunk;
pub mod clean;
pub mod discourse;
pub mod document;
pub mod embed;
pub mod lock;
pub mod error;
pub mod spec;
pub mod store;

pub use chunk::chunk;
pub use clean::strip_quote_blocks;
pub use discourse::parse_topic;
pub use document::Document;
pub use embed::Embedder;
pub use error::CoreError;
pub use lock::WriterLock;
pub use store::{SCHEMA_VERSION, SearchHit, Store};
