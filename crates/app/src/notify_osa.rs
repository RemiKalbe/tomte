//! macOS notifications via `osascript` (spec §7.6; plan 5 Task 7).
//!
//! The daemon already filters expected/self-caused events, so the app only
//! turns *news* into notifications: main.rs coalesces gated new-drift pushes
//! into 5s windows and fires one "N file(s) drifted" per window, and forwards
//! `RemoteAdvanced` pushes as "origin advanced: <file>". Nothing fires for
//! applied/expected events. [`notify`] blocks on the subprocess, so callers
//! keep it on the background executor (spec §3.2).

use std::path::Path;

use czui_core::cmd::{CommandRequest, CommandRunner};

/// Escape a string for a double-quoted AppleScript literal. Backslash and
/// double quote are the only metacharacters that matter: the script reaches
/// `osascript` as a single argv element (no shell in between), so nothing
/// else can break out of the literal.
pub fn escape_applescript(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// The `-e` script line: `display notification "<body>" with title "<title>"`.
pub fn notification_script(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(body),
        escape_applescript(title)
    )
}

/// Post one user notification. Blocking (runs `osascript`) — callers keep it
/// on the background executor. Failures are logged, never fatal: a missing or
/// sandboxed osascript must not take down the event loop.
pub fn notify(runner: &dyn CommandRunner, title: &str, body: &str) {
    let req = CommandRequest::new("osascript")
        .arg("-e")
        .arg(notification_script(title, body));
    match runner.run(req) {
        Ok(out) if !out.success() => {
            eprintln!(
                "chezmoi-ui: osascript exited {}: {}",
                out.exit_code,
                out.stderr_utf8().trim()
            );
        }
        Ok(_) => {}
        Err(e) => eprintln!("chezmoi-ui: notification failed: {e}"),
    }
}

/// Body for one coalesced drift window.
pub fn drift_body(n: usize) -> String {
    if n == 1 {
        "1 file drifted".to_string()
    } else {
        format!("{n} files drifted")
    }
}

/// Body for a `RemoteAdvanced` push: origin moved for this target.
pub fn remote_advanced_body(target: &Path) -> String {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    format!("origin advanced: {name}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use czui_core::cmd::CommandError;
    use czui_core::cmd::fake::FakeRunner;

    use super::*;

    #[test]
    fn escape_handles_quotes_and_backslashes() {
        let cases = [
            ("plain text", "plain text"),
            (r#"say "hi""#, r#"say \"hi\""#),
            (r"C:\path\file", r"C:\\path\\file"),
            (r#"mix \" of both"#, r#"mix \\\" of both"#),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(escape_applescript(input), want, "input {input:?}");
        }
    }

    #[test]
    fn script_places_escaped_body_and_title() {
        assert_eq!(
            notification_script(r#"ti"tle"#, r"bo\dy"),
            r#"display notification "bo\\dy" with title "ti\"tle""#
        );
    }

    #[test]
    fn notify_shells_osascript_with_one_e_flag() {
        let fake = FakeRunner::new();
        fake.push_ok(0, "", "");
        notify(&fake, "chezmoi-ui", "2 files drifted");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "osascript");
        assert_eq!(
            calls[0].args,
            vec![
                "-e".to_string(),
                r#"display notification "2 files drifted" with title "chezmoi-ui""#.to_string(),
            ]
        );
    }

    #[test]
    fn notify_never_panics_on_failure() {
        // non-zero exit (e.g. notifications denied)
        let fake = FakeRunner::new();
        fake.push_ok(1, "", "not allowed");
        notify(&fake, "t", "b");

        // spawn failure (osascript missing entirely)
        let fake = FakeRunner::new();
        fake.push_err(CommandError::Spawn {
            program: "osascript".into(),
            source: std::io::Error::other("no such file"),
        });
        notify(&fake, "t", "b");
    }

    #[test]
    fn drift_body_pluralizes() {
        assert_eq!(drift_body(1), "1 file drifted");
        assert_eq!(drift_body(2), "2 files drifted");
        assert_eq!(drift_body(10), "10 files drifted");
    }

    #[test]
    fn remote_advanced_body_uses_file_name() {
        assert_eq!(
            remote_advanced_body(Path::new("/home/u/.zshrc")),
            "origin advanced: .zshrc"
        );
        // no file name (root) falls back to the full display path
        assert_eq!(remote_advanced_body(Path::new("/")), "origin advanced: /");
    }
}
