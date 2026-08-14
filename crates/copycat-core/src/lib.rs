//! copycat-core — shared types and policy for copycat, a temporal clipboard:
//! entries are timestamped and "the current clipboard" is simply the most
//! recent one. This crate is platform-agnostic — no OS clipboard, no async
//! runtime; those live in `copycatd`. The re-exports below are the intended
//! public surface.

pub mod detect;
pub mod entry;
pub mod insecure;
pub mod ipc;
pub mod pii;
pub mod privacy;
pub mod proto;
pub mod store;
pub mod transform;
pub mod usage;
pub mod view;
pub mod wire;

// The names the workspace imports across the crate boundary.
pub use crate::entry::{Content, ContentKind, Entry, Sensitivity, Source};
pub use crate::privacy::Mode as PrivacyMode;
pub use crate::proto::{socket_path, Request, Response, Selector};
pub use crate::store::{ClipboardStore, MemoryStore, Recall};
pub use crate::transform::Transform;
pub use crate::view::Audience;
