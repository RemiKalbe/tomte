//! Embedded asset source: gpui's `svg()` element loads icon paths through
//! this (registered via `Application::with_assets`). Icons are compiled in —
//! no bundle lookup, no missing-file states.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// `(virtual path, bytes)` for every embedded asset.
const ASSETS: &[(&str, &[u8])] = &[(
    "icons/chevron_down.svg",
    include_bytes!("../assets/icons/chevron_down.svg"),
)];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::new_static(p))
            .collect())
    }
}
