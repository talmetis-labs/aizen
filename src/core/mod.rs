//! Cross-cutting foundation shared by every other module: the wire/data `types`, the
//! endpoint `config` + CLI-state `cli_config`, and the outbound `net_guard`. No business
//! logic lives here — this is the layer everything else is allowed to depend on.

pub mod approval;
pub mod aside;
pub mod cancel;
pub mod cli_config;
pub mod config;
pub mod convo;
pub mod device;
pub mod effort;
pub mod endpoint;
pub mod exec_ctx;
pub mod file_placement;
pub mod gitx;
pub(crate) mod native_journal;
pub(crate) mod native_scope;
pub mod net_guard;
pub mod persist;
pub mod proctree;
pub mod read_ledger;
pub mod recovery;
pub mod reference_media;
pub mod repo_lock;
pub mod scratch;
pub mod session_store;
pub mod slug;
pub(crate) mod speculation;
pub mod steer;
pub mod tokens;
pub mod tool_media;
pub mod turn_shape;
pub mod types;
pub mod workspace_txn;
