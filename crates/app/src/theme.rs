//! GitHub-palette semantic tokens (spec §7.5). Light and dark from day one.

use gpui::{Rgba, WindowAppearance, rgb};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Rgba,
    pub surface: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub accent: Rgba,
    pub ok: Rgba,
    pub drift: Rgba,
    pub conflict: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x0d1117),
            surface: rgb(0x161b22),
            border: rgb(0x30363d),
            text: rgb(0xc9d1d9),
            text_muted: rgb(0x8b949e),
            accent: rgb(0x58a6ff),
            ok: rgb(0x3fb950),
            drift: rgb(0xd29922),
            conflict: rgb(0xf85149),
        }
    }

    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            surface: rgb(0xf6f8fa),
            border: rgb(0xd0d7de),
            text: rgb(0x1f2328),
            text_muted: rgb(0x656d76),
            accent: rgb(0x0969da),
            ok: rgb(0x1a7f37),
            drift: rgb(0x9a6700),
            conflict: rgb(0xcf222e),
        }
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    pub fn class_color(&self, class: &str) -> Rgba {
        match class {
            "conflict" | "local_source_diverged" | "eval_failed" => self.conflict,
            "destination_drift" | "source_ahead" => self.drift,
            "remote_ahead" => self.accent,
            _ => self.ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_colors_are_distinct_per_severity() {
        let t = Theme::dark();
        assert_eq!(t.class_color("conflict"), t.conflict);
        assert_eq!(t.class_color("destination_drift"), t.drift);
        assert_eq!(t.class_color("remote_ahead"), t.accent);
        assert_eq!(t.class_color("in_sync"), t.ok);
    }
}
