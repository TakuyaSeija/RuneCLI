//! Finding and matching: how the engine looks for things on disk.
//!
//! Three pieces, each written in-tree because the crates that do these jobs are
//! built for workloads an order of magnitude larger than one project directory:
//!
//!   * [`walk`] — a bounded directory walk that honours `.gitignore`
//!   * [`glob`] — path patterns (`src/**/*.rs`)
//!   * [`rx`]   — a small bounded regex, for content search
//!
//! Nothing here touches the jail; callers resolve paths first.

pub mod glob;
pub mod rx;
pub mod walk;
