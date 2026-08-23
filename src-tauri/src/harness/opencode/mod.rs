//! **OpenCode's L1** — everything cImp knows about one harness, in one
//! directory (V35 Phase K, design § 4).
//!
//! | Module | Direction | What it owns |
//! |---|---|---|
//! | [`plugin`] | cImp ▸ harness | the generated `.opencode/plugin/cimp-inject-<tab>.js`: its source, the per-tab flags baked into it, when it is written or swept, and the CHP hello it opens with |
//! | [`tools`] | cImp ▸ harness | the reviewed table of the harness's OWN tool ids — the names the generated plugin's gate and beacon match on |
//! | [`read`] | harness ▸ cImp | the LEGACY fallback reader — the `GET /event` SSE tap (Tier C, retired from the hot path by Phase L) |
//!
//! The generated plugin is inside the TCB (design § 5, D7): the V32 Phase H
//! native-tool refusal is a `throw` in its `tool.execute.before`, and only the
//! plugin sits in the harness's own tool path. A change to [`plugin`] or
//! [`tools`] is a change to a security control, not to a data pipe.

pub mod canary;
pub mod config;
pub mod harness_plugin;
pub mod input;
pub mod plugin;
pub mod probe;
pub mod prompts;
pub mod read;
pub mod settings;
pub mod tools;

pub use harness_plugin::PLUGIN;
