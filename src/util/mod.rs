//! Small, domain-free helpers.
//!
//! Nothing here knows what a node, a run, or a workspace is — each file is a
//! self-contained function over plain strings and numbers that happened to be
//! needed by one of the real modules. They live together so the domain
//! modules read as domain logic, and so a single implementation is shared
//! rather than copied (`now_ms` was verbatim in two files before this).
//!
//! - [`env_file`] — `.env`-style parsing.
//! - [`label`] — munging arbitrary text into a `[a-z0-9-]` slug.
//! - [`mime`] — content type by file extension.
//! - [`query`] — looking up a key in a `k=v&k=v` query string.
//! - [`time`] — the current unix time in milliseconds.

pub mod env_file;
pub mod label;
pub mod mime;
pub mod query;
pub mod time;
