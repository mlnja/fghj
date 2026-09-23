//! Everything `fghj` exposes over HTTP — the three surfaces a client can
//! reach, plus the machinery that makes them reachable.
//!
//! - [`api`] — the axum control API the `fghj` CLI and the browser UI both
//!   call. Served on a Unix socket (CLI) and an ephemeral loopback TCP port
//!   (relayed to by the proxy); see `daemon::bootstrap::run_control_api`.
//! - [`ui`] — the embedded Svelte bundle, served as the API router's
//!   fallback route.
//! - [`proxy`] — the TLS reverse proxy on 80/443 (SPEC.md Subsystem C) that
//!   turns `https://<node>.fghj.internal` into a request against the right
//!   container's published port.
//! - [`ca`] — the local certificate authority the proxy mints leaf certs
//!   from, and its installation into the system/container trust stores.
//!   Exists solely so those `https://` URLs are trusted rather than
//!   click-through-warning.
//!
//! The web-only helpers live here too, rather than in `util`, because each
//! one only makes sense in terms of HTTP: [`mime`] (content type by
//! extension) and [`query`] (reading a key out of a query string). `util`
//! stays for helpers that would still make sense in a `fghj` with no HTTP
//! surface at all.
//!
//! What's deliberately *not* here: `dns` (a different protocol and a
//! different SPEC subsystem, even though the proxy is what its answers point
//! at) and `daemon::routing` (the glue impl'ing [`proxy::RouteResolver`] for
//! `WorkspaceRegistry` — it belongs with the registry it reads, not with the
//! trait it satisfies).

pub mod api;
pub mod ca;
pub mod mime;
pub mod proxy;
pub mod query;
pub mod ui;
