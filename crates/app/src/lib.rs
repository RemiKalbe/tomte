//! tomte-app library target: the shared, gpui-light modules (theme tokens,
//! pure sync model, IPC client) consumed by both the `tomte` binary and
//! integration tests. Views and the AppKit platform layer stay bin-only.

pub mod ipc;
pub mod merge_inputs;
pub mod merge_state;
pub mod model;
pub mod resolve;
/// Re-export: the theme moved into the tomte-ui design-system crate; every
/// existing `tomte_app::theme::Theme` import keeps working.
pub use tomte_ui::theme;
