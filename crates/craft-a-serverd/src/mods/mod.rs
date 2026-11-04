//! Mod management through Modrinth (Phase 3).
//!
//! * [`http`]: the network behind a trait, URL policy, rate limiter.
//! * [`client`]: typed Modrinth calls and the verified download.
//! * [`model`]: API response types (untrusted input).
//! * [`resolver`]: pure dependency resolution into an install plan.
//! * [`files`]: safe file names, hashing writer, jar structure check, inventory.
//! * [`txn`]: journal, staging and the crash-safe file moves.
//! * [`logic`]: small pure helpers (naming, orphans, conflict text).
//!
//! The transaction that ties these to the server lifecycle (snapshot, stop,
//! move, restart, health check, rollback) is `Service` code in
//! `lifecycle/mods.rs`.
//!
//! Mods are untrusted code. Nothing in the daemon loads, unpacks or runs a
//! jar; the game container is the only place they execute.

pub mod client;
pub mod error;
pub mod files;
pub mod http;
pub mod logic;
pub mod model;
pub mod resolver;
pub mod txn;

#[cfg(test)]
pub(crate) mod testutil;

pub use client::{CATEGORIES, ClientSource, ModrinthClient, SearchQuery, SearchSort, user_agent};
pub use error::ModError;
pub use http::{HttpError, HttpMeta, HttpRequest, Method, Transport, UreqTransport};

/// A client that talks to the real Modrinth over HTTPS.
pub fn real_client(contact: Option<&str>) -> ModrinthClient {
    ModrinthClient::new(std::sync::Arc::new(UreqTransport::new(&user_agent(
        contact,
    ))))
}
