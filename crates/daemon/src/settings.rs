//! Shared app settings (spec §9): read by daemon and app.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub fetch_interval_minutes: u64,
    pub onepassword_account: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fetch_interval_minutes: 15,
            onepassword_account: None,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "tomted: invalid settings at {}: {e}; using defaults",
                        path.display()
                    );
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    /// Environment injected into every chezmoi/op subprocess (spec §9):
    /// never interactive, account selection comes from settings.
    pub fn chezmoi_env(&self) -> Vec<(String, String)> {
        match &self.onepassword_account {
            Some(acct) => vec![("OP_ACCOUNT".to_string(), acct.clone())],
            None => Vec::new(),
        }
    }
}

pub fn app_support_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = home.join("Library/Application Support/Tomte");
    // One-time migration from the app's pre-rename identity (2026-08-08:
    // chezmoi-ui → Tomte): adopt the old directory wholesale — the journal
    // holds history and undo snapshots that must not be silently orphaned.
    if !dir.exists() {
        let old = home.join("Library/Application Support/ChezmoiUI");
        if old.exists() {
            let _ = std::fs::rename(&old, &dir);
        }
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing() {
        let s = Settings::load(std::path::Path::new("/nonexistent/settings.toml"));
        assert_eq!(s.fetch_interval_minutes, 15);
        assert!(s.onepassword_account.is_none());
        assert!(s.chezmoi_env().is_empty());
    }

    #[test]
    fn parses_and_injects_op_account() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        std::fs::write(
            &p,
            "fetch_interval_minutes = 5\nonepassword_account = \"my.acct\"\n",
        )
        .unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.fetch_interval_minutes, 5);
        assert_eq!(
            s.chezmoi_env(),
            vec![("OP_ACCOUNT".to_string(), "my.acct".to_string())]
        );
    }
}
