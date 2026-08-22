//! **Claude Code's L1** — everything cImp knows about one harness, in one
//! directory (V35 Phase K, design § 4).
//!
//! One file per direction the seam runs in:
//!
//! | Module | Direction | What it owns |
//! |---|---|---|
//! | [`overlay`] | cImp ▸ harness | the generated `--settings` overlay: which hooks this tab wires, and the CHP hello that declares them |
//! | [`hook`] | harness ▸ cImp | the `type: "http"` hook payloads the harness POSTs back, the emitted hook entry that points at them, **and the routes and handlers that receive them** (V40 Phase C) |
//! | [`prompts`] | harness ▸ cImp | this TUI's prompt grammar — the substrings the neutral `PermissionDetector` matches on, and the reasoning behind each |
//! | [`read`] | harness ▸ cImp | the LEGACY fallback reader — the transcript JSONL tail (Tier C, retired from the hot path by Phase L) |
//! | [`statusline`] | harness ▸ cImp | the Claude-shaped stdin payload `cimp --statusline` is handed (Tier C, same) |
//!
//! What this harness serves is declared in [`overlay::claude_hello`] and, per
//! capability, in [`crate::harness::contract::capabilities`]. Adding a harness
//! is adding a sibling of this directory — see `harness/README.md`.

pub mod canary;
pub mod hook;
pub mod input;
pub mod overlay;
pub mod probe;
pub mod prompts;
pub mod read;
pub mod settings;
pub mod statusline;
pub mod tools;

pub mod plugin;

pub use plugin::PLUGIN;
