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
    /// Origin's version of a source file — the fourth content state.
    /// Pastel purple: a member of no existing family, because origin is
    /// neither disk (amber) nor local source (blue).
    pub remote: Rgba,
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
            remote: rgb(0xd2a8ff),
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
            remote: rgb(0x8250df),
        }
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    /// A translucent wash of a token — hover layers and chip backgrounds
    /// (Zed layers ghost elements the same way).
    pub fn wash(c: Rgba, a: f32) -> Rgba {
        Rgba { a, ..c }
    }

    pub fn class_color(&self, class: &str) -> Rgba {
        match class {
            "conflict" | "local_source_diverged" | "eval_failed" => self.conflict,
            "destination_drift" | "source_ahead" | "type_changed" => self.drift,
            "remote_ahead" => self.accent,
            _ => self.ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MUTATION: drop "type_changed" from `class_color`'s drift arm → it falls to
    /// `ok`, the in-sync green.
    #[test]
    fn class_colors_are_distinct_per_severity() {
        let t = Theme::dark();
        assert_eq!(t.class_color("conflict"), t.conflict);
        assert_eq!(t.class_color("destination_drift"), t.drift);
        // A type change is disk drift with one decision; green would read as in sync.
        assert_eq!(t.class_color("type_changed"), t.drift);
        assert_eq!(t.class_color("remote_ahead"), t.accent);
        assert_eq!(t.class_color("in_sync"), t.ok);
    }

    #[test]
    fn remote_is_distinct_from_every_other_class_tint() {
        // Origin is its own provenance and must never be confused with the
        // local source (accent), disk (drift), or a resolved region (ok).
        for t in [Theme::dark(), Theme::light()] {
            for other in [t.accent, t.drift, t.ok, t.conflict, t.text_muted] {
                assert_ne!(
                    (t.remote.r, t.remote.g, t.remote.b),
                    (other.r, other.g, other.b),
                    "remote must be visually distinct from every other tint"
                );
            }
        }
    }
}
