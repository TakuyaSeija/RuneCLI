//! **Rune** — a local agentic coding engine.
//!
//! One engine, driven by any frontend: `RuneCli` today, `RuneStudio` later. It
//! runs entirely on this machine: sessions, undo history, permission grants and
//! the workspace jail never leave it. The only traffic that exists at all goes
//! through [`net`] — the model provider the user configured, and the web-search
//! tool — with the user's own keys, to no server of ours.
//!
//! # Shape
//!
//! ```text
//! frontend ──Command──► engine ──► session ──► provider (model)
//!          ◄──Event───            │
//!                                 ├──► perm    may this happen?
//!                                 ├──► tools   do it
//!                                 │      ├──► jail       paths stay inside
//!                                 │      └──► sandbox    kernel confines commands
//!                                 ├──► checkpoint  undo before every write
//!                                 └──► store       sessions, grants, manifests
//! ```
//!
//! # The three rules the code enforces rather than documents
//!
//! 1. **The engine decides.** A tool call is a request whoever it came from.
//!    [`perm::Gate`] is the only path to running one.
//! 2. **No write without an undo.** [`tools::ToolCtx::write_guard`] is the only
//!    way a tool reaches the filesystem, and it captures first.
//! 3. **No silent downgrade.** When the kernel sandbox cannot be applied, the
//!    `ready` event says so ([`proto::SandboxReport`]).
//! 4. **A bug costs one call, not the session.** A panic inside a tool is caught
//!    ([`panic`]) and reported as a failed tool result; the busy flag is released
//!    by a guard rather than by a line the unwind would skip.

pub mod access;
#[path = "Capabilities/Mod.rs"]
pub mod capabilities;
pub mod checkpoint;
pub mod engine;
pub mod error;
pub mod frame;
pub mod jail;
pub mod limits;
pub mod lock;
pub mod media;
pub mod memory;
pub mod net;
pub mod panic;
pub mod perm;
pub mod platform;
#[path = "Project.rs"]
pub mod project;
pub mod proto;
pub mod provider;
pub mod retry;
pub mod sandbox;
pub mod scan;
pub mod session;
pub mod settings;
pub mod shell;
pub mod store;
pub mod tasks;
pub mod terminal_screen;
pub mod tools;
pub mod upgrade;
pub mod web;

pub use engine::{Config, Engine, VERSION};
pub use error::{Error, Result};
pub use proto::{Command, Event};
