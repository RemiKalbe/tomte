//! czui-app library target: the shared, gpui-light modules (theme tokens,
//! pure sync model, IPC client) consumed by both the `chezmoi-ui` binary and
//! integration tests. Views and the AppKit platform layer stay bin-only.

pub mod ipc;
pub mod merge_inputs;
pub mod merge_state;
pub mod model;
pub mod resolve;
pub mod theme;
