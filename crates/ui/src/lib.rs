//! tomte-ui: the design system. Theme tokens plus every reusable component,
//! one file per component — the single styling source for all views and for
//! the gallery's component-isolation previews (`tomte --gallery
//! comp:<name>`).
//!
//! Components are pure builders: `Theme` + data + plain click handlers
//! (`impl Fn(&ClickEvent, &mut Window, &mut App)`), never gpui
//! Entity/Context coupling — so they render identically in place and in
//! isolation.

pub mod assets;
pub mod components;
pub mod preview;
pub mod theme;

pub use assets::Assets;
pub use components::banner::BannerTint;
pub use theme::Theme;
