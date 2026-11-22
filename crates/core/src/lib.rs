//! Backend of a desktop key browser for Redis and Valkey.
//!
//! Connection manager, capability layer, cluster routing, `SCAN` engine,
//! the SQLite keyspace index, search, write-through mutations and
//! keyspace notifications. No UI lives here; the crate is driven by the
//! CLI harness in `crates/cli` and by the Tauri shell in `src-tauri`.
//!
//! Invariants (see `CONCEPT.md`):
//! - the keyspace lives in the index, callers read windows of at most 200 rows
//! - the server is the truth, the index is a rebuildable cache
//! - enumeration is `SCAN` only, with adaptive `COUNT`, pausable
//! - every command is a raw command built in [`commands`]
//! - no feature is gated on `redis_version`

pub mod browse;
pub mod capability;
pub mod cluster;
pub mod commands;
pub mod connection;
pub mod console;
pub mod error;
pub mod fold;
pub mod glob;
pub mod index;
pub mod meta;
pub mod mutate;
pub mod notify;
pub mod private;
pub mod route;
pub mod scan;
pub mod search;
pub mod segment;
pub mod sysinfo;
pub mod tunnel;
pub mod value;

pub use capability::{Capabilities, CommandState, ServerKind};
pub use cluster::{NodeAddr, Role, Topology};
pub use connection::{Conn, ConnectionManager, IndexMode, Profile, ScanTarget, Tag};
pub use error::{Error, Result};
pub use index::{Anchors, Index, IndexOptions, Location, Node, Row, ScanState, ScanStatus, SearchPage, Sort};
pub use route::{Databases, Router};
pub use scan::{ScanConfig, ScanHandle, ScanNode, ScanOutcome, ScanPhase, ScanProfile, ScanProgress};
