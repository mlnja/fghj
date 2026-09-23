//! Everything that reads or writes durable state outside process memory:
//! per-workspace SQLite (`sqlite`), the two plain-JSON side files
//! (`index`, `daemon_state`), and the captured workspace-owner identity
//! (`workspace_owner`). Per the migration plan's persistence design:
//! SQLite/JSON are write-through (mutated only alongside the in-memory
//! state change that motivated them) and read-once-at-boot
//! (`rehydrate::rehydrate`) — nothing reads them live during normal
//! operation, except `logs`/`events` (`sqlite::logs`/`sqlite::events`),
//! which are a deliberate, narrow exception: unbounded append-only
//! telemetry nothing ever branches a decision on, so they stay direct-write
//! /direct-read, untouched by this module's write-through/read-once
//! discipline.
//!
//! **Deviation from the original target module layout, and why**: the plan
//! called for a dedicated `effects/persistence.rs` `Effect` impl to become
//! the sole writer of run/container state, diffing `WorkspaceState` and
//! writing through on change. That's *not* built here. `RunRegistry`
//! (`src/runs.rs`) was left partially alive by migration phase 5 — its
//! `start`/`restart_container`/`stop_container`/`remove_container`/`stop`
//! methods are still called wholesale (by design — see `effects::docker`'s
//! module docs on why the sidecar-orchestration ordering inside them must
//! not be decomposed) and each already calls `sqlite::WorkspaceDb::save_run`
//! /`delete_run` inline as part of doing its real Docker work. Adding a
//! second, independent `Effect`-based writer on top would create exactly
//! the "two mechanisms driving the same resource concurrently" hazard this
//! whole refactor exists to eliminate — the two writers could race, or the
//! Effect's diff-based write could fire against a state that doesn't yet
//! reflect what the in-flight Docker call is about to persist itself.
//! `RunRegistry`'s own methods therefore remain the sole writers of
//! run/container SQLite state until a later phase actually retires
//! `RunRegistry` as a stateful struct — at which point the write can move
//! into a real `Effect` with nothing left to race against.
//!
//! For the same reason, `rehydrate::rehydrate` is *not* independently wired
//! into the new actor system's seeding path — `RunRegistry::new` is its
//! sole caller. The new-system actor (`daemon.rs`'s `wire_actor`) seeds
//! itself by re-keying `RunRegistry`'s own already-reconciled `.list()`
//! (see `daemon::WorkspaceRegistry::wire_actor`), not by reading SQLite a
//! second time — `rehydrate` doesn't just read rows, it also reconciles
//! each container against live Docker status and prunes dead ones, so a
//! second, independent caller would either have to duplicate that
//! reconciliation or skip it, either of which produces a boot-time view of
//! reality that can diverge from `RunRegistry`'s.
//!
//! `index.json` (`index`) and `daemon-state.json` (`daemon_state`) aren't
//! part of this tension: they're registry/daemon-level infrastructure, not
//! per-workspace decision state modeled by any `Action`/reducer (per the
//! plan's own "registry: infrastructure, not decision state" section), and
//! are already written through at exactly the point of the relevant
//! mutation (workspace registration/deregistration; `daemon start`/`stop`).

pub mod daemon_state;
pub mod index;
pub mod rehydrate;
pub mod sqlite;
pub mod workspace_owner;

mod paths;

pub use daemon_state::{DaemonState, load_daemon_state, save_daemon_state};
pub use index::{load_index, save_index};
pub use paths::{default_index_path, default_state_path, fghjd_root};
pub use rehydrate::rehydrate;
pub use sqlite::{EventEntry, LogGeneration, LogLine, WorkspaceDb};
pub use workspace_owner::{WorkspaceOwner, harden_git_ssh};
