//! Structured (per-key) merge support for JSON and JSONC files.

pub mod jsonc;
pub mod merge;
pub mod splice;

pub use merge::{
    Change, Decision, Format, Lock, NotStructured, Pick, Resolution, Side, SideText,
    StructuredDocument, detect, looked_like_json,
};
pub use splice::AssembleError;
