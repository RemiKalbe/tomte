//! ChezmoiClient + parsers + error classification.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::cmd::{CommandError, CommandRequest, CommandRunner};

#[derive(Clone)]
pub struct ChezmoiOptions {
    pub bin: String,
    pub base_args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl Default for ChezmoiOptions {
    fn default() -> Self {
        Self {
            bin: "chezmoi".into(),
            // Never let a subprocess sit on an interactive prompt (spec §9):
            // a locked 1Password otherwise turns every templated call into a
            // 30s hang instead of a fast, classifiable failure.
            base_args: vec!["--no-tty".into(), "--no-pager".into()],
            env: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
pub struct ChezmoiClient {
    runner: Arc<dyn CommandRunner>,
    opts: ChezmoiOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeCode {
    None,
    Added,
    Deleted,
    Modified,
    Run,
}

impl ChangeCode {
    fn from_char(c: char) -> Option<Self> {
        match c {
            ' ' => Some(Self::None),
            'A' => Some(Self::Added),
            'D' => Some(Self::Deleted),
            'M' => Some(Self::Modified),
            'R' => Some(Self::Run),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub last_written_vs_actual: ChangeCode,
    pub actual_vs_target: ChangeCode,
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct StateDump {
    #[serde(default, rename = "entryState")]
    pub entry_state: HashMap<PathBuf, EntryState>,
}

#[derive(Debug, Deserialize)]
pub struct EntryState {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, rename = "contentsSHA256")]
    pub contents_sha256: Option<String>,
    #[serde(default)]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalFailureKind {
    OnePasswordMultipleAccounts,
    OnePasswordAuth,
    AgeDecrypt,
    GpgDecrypt,
    TemplateError,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct EvalFailure {
    pub kind: EvalFailureKind,
    pub raw_stderr: String,
    pub hint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ChezmoiError {
    #[error("template/secret evaluation failed: {}", .0.hint)]
    Eval(EvalFailure),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("chezmoi exited with code {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("failed to parse chezmoi output ({context}): {detail}")]
    Parse {
        context: &'static str,
        detail: String,
    },
}

/// Classify stderr from a failed chezmoi invocation into a structured
/// evaluation failure (spec §4 EvalFailed, §9 remediation hints).
pub fn classify_eval_stderr(stderr: &str) -> Option<EvalFailure> {
    let mk = |kind, hint: &str| {
        Some(EvalFailure {
            kind,
            raw_stderr: stderr.to_string(),
            hint: hint.to_string(),
        })
    };
    if stderr.contains("multiple accounts found") {
        return mk(
            EvalFailureKind::OnePasswordMultipleAccounts,
            "Select a 1Password account in Settings (sets OP_ACCOUNT for all chezmoi calls).",
        );
    }
    if stderr.contains("onepassword") || stderr.contains("op signin") {
        return mk(
            EvalFailureKind::OnePasswordAuth,
            "1Password CLI could not authenticate. Unlock 1Password and retry.",
        );
    }
    if stderr.contains("age:") {
        return mk(
            EvalFailureKind::AgeDecrypt,
            "age decryption failed — check identity file.",
        );
    }
    if stderr.contains("gpg:") {
        return mk(
            EvalFailureKind::GpgDecrypt,
            "gpg decryption failed — check keyring.",
        );
    }
    if stderr.contains("template:") {
        return mk(
            EvalFailureKind::TemplateError,
            "Template failed to render — see raw error.",
        );
    }
    None
}

impl ChezmoiClient {
    pub fn new(runner: Arc<dyn CommandRunner>, opts: ChezmoiOptions) -> Self {
        Self { runner, opts }
    }

    fn request(&self, args: &[&str]) -> CommandRequest {
        let mut req = CommandRequest::new(&self.opts.bin)
            .args(self.opts.base_args.iter().cloned())
            .args(args.iter().map(|s| s.to_string()))
            .timeout(self.opts.timeout);
        for (k, v) in &self.opts.env {
            req = req.env(k.clone(), v.clone());
        }
        req
    }

    fn run_ok(&self, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Vec<u8>, ChezmoiError> {
        self.run_ok_with_timeout(args, stdin, self.opts.timeout)
    }

    fn run_ok_with_timeout(
        &self,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Vec<u8>, ChezmoiError> {
        let mut req = self.request(args).timeout(timeout);
        if let Some(bytes) = stdin {
            req = req.stdin(bytes);
        }
        let out = self.runner.run(req)?;
        if out.success() {
            return Ok(out.stdout);
        }
        let stderr = out.stderr_utf8();
        if let Some(eval) = classify_eval_stderr(&stderr) {
            return Err(ChezmoiError::Eval(eval));
        }
        Err(ChezmoiError::Exit {
            code: out.exit_code,
            stderr,
        })
    }

    fn run_utf8(&self, args: &[&str]) -> Result<String, ChezmoiError> {
        Ok(String::from_utf8_lossy(&self.run_ok(args, None)?).into_owned())
    }

    pub fn source_dir(&self) -> Result<PathBuf, ChezmoiError> {
        Ok(PathBuf::from(self.run_utf8(&["source-path"])?.trim()))
    }

    pub fn managed(&self) -> Result<Vec<PathBuf>, ChezmoiError> {
        Ok(self
            .run_utf8(&["managed", "--path-style=absolute"])?
            .lines()
            .map(PathBuf::from)
            .collect())
    }

    pub fn status(&self) -> Result<Vec<StatusEntry>, ChezmoiError> {
        let text = self.run_utf8(&["status", "--path-style=absolute"])?;
        let mut entries = Vec::new();
        for line in text.lines() {
            if line.len() < 4 {
                continue;
            }
            let mut chars = line.chars();
            let c1 = chars.next().and_then(ChangeCode::from_char);
            let c2 = chars.next().and_then(ChangeCode::from_char);
            let (Some(c1), Some(c2)) = (c1, c2) else {
                return Err(ChezmoiError::Parse {
                    context: "status",
                    detail: line.to_string(),
                });
            };
            entries.push(StatusEntry {
                last_written_vs_actual: c1,
                actual_vs_target: c2,
                path: PathBuf::from(&line[3..]),
            });
        }
        Ok(entries)
    }

    pub fn cat(&self, target: &Path) -> Result<Vec<u8>, ChezmoiError> {
        self.run_ok(&["cat", &target.to_string_lossy()], None)
    }

    pub fn source_path(&self, target: &Path) -> Result<PathBuf, ChezmoiError> {
        Ok(PathBuf::from(
            self.run_utf8(&["source-path", &target.to_string_lossy()])?
                .trim(),
        ))
    }

    pub fn target_paths(&self, sources: &[PathBuf]) -> Result<Vec<PathBuf>, ChezmoiError> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = vec!["target-path".into()];
        args.extend(sources.iter().map(|p| p.to_string_lossy().into_owned()));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Ok(self
            .run_utf8(&arg_refs)?
            .lines()
            .map(PathBuf::from)
            .collect())
    }

    pub fn state_dump(&self) -> Result<StateDump, ChezmoiError> {
        let bytes = self.run_ok(&["state", "dump", "--format=json"], None)?;
        serde_json::from_slice(&bytes).map_err(|e| ChezmoiError::Parse {
            context: "state dump",
            detail: e.to_string(),
        })
    }

    pub fn execute_template(&self, input: &[u8]) -> Result<Vec<u8>, ChezmoiError> {
        self.run_ok(&["execute-template"], Some(input.to_vec()))
    }

    pub fn apply(&self, target: Option<&Path>) -> Result<(), ChezmoiError> {
        match target {
            Some(t) => self.run_ok(&["apply", &t.to_string_lossy()], None)?,
            None => self.run_ok(&["apply"], None)?,
        };
        Ok(())
    }

    /// Re-add a modified target into the source state. NOTE: chezmoi silently
    /// ignores templated sources — callers must pre-detect `.tmpl` and refuse.
    pub fn re_add(&self, target: &Path) -> Result<(), ChezmoiError> {
        self.run_ok(&["re-add", &target.to_string_lossy()], None)?;
        Ok(())
    }

    /// Pull changes from the source repo and apply them. Network op: 120s
    /// timeout like git fetch, overriding the configured default.
    pub fn update(&self) -> Result<(), ChezmoiError> {
        self.run_ok_with_timeout(&["update"], None, Duration::from_secs(120))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::fake::FakeRunner;
    use std::sync::Arc;

    const OP_MULTI_ACCOUNT_STDERR: &str = concat!(
        "[ERROR] 2026/07/19 23:12:19 multiple accounts found. Use the --account flag or set the OP_ACCOUNT environment variable to select an account.\n",
        "chezmoi: .config/nushell/env.nu: template: dot_config/nushell/env.nu.tmpl:117:29: executing \"dot_config/nushell/env.nu.tmpl\" at <onepasswordRead \"op://Personal/x/hostname\">: error calling onepasswordRead: /opt/homebrew/bin/op signin --raw: exit status 1\n",
    );

    fn client(fake: Arc<FakeRunner>) -> ChezmoiClient {
        ChezmoiClient::new(fake, ChezmoiOptions::default())
    }

    #[test]
    fn parses_status_lines() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, " M /Users/x/.config/starship.toml\nMM /Users/x/.zshrc\n A /Users/x/.newfile\n R /Users/x/install.sh\n", "");
        let entries = client(fake.clone()).status().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].last_written_vs_actual, ChangeCode::None);
        assert_eq!(entries[0].actual_vs_target, ChangeCode::Modified);
        assert_eq!(entries[1].last_written_vs_actual, ChangeCode::Modified);
        assert_eq!(entries[3].actual_vs_target, ChangeCode::Run);
        // status must be invoked with absolute paths
        let call = &fake.calls()[0];
        assert!(call.args.contains(&"--path-style=absolute".to_string()));
    }

    #[test]
    fn classifies_1password_multiple_accounts() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(1, "", OP_MULTI_ACCOUNT_STDERR);
        let err = client(fake).status().unwrap_err();
        match err {
            ChezmoiError::Eval(f) => {
                assert_eq!(f.kind, EvalFailureKind::OnePasswordMultipleAccounts);
                assert!(f.hint.contains("OP_ACCOUNT"));
            }
            other => panic!("expected Eval, got {other:?}"),
        }
    }

    #[test]
    fn parses_state_dump_entry_state() {
        // Shape captured from `chezmoi state dump --format=json` on 2026-07-19.
        let json = r#"{
          "configState": {"configState": {"configTemplateContentsSHA256": "801a"}},
          "entryState": {
            "/Users/x/.Brewfile": {"contentsSHA256": "907b", "mode": 420, "type": "file"},
            "/Users/x/.agents": {"mode": 2147484141, "type": "dir"}
          },
          "scriptState": {}
        }"#;
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, json, "");
        let dump = client(fake).state_dump().unwrap();
        let f = &dump.entry_state[std::path::Path::new("/Users/x/.Brewfile")];
        assert_eq!(f.kind, "file");
        assert_eq!(f.contents_sha256.as_deref(), Some("907b"));
        let d = &dump.entry_state[std::path::Path::new("/Users/x/.agents")];
        assert_eq!(d.kind, "dir");
        assert_eq!(d.contents_sha256, None);
    }

    #[test]
    fn base_args_and_env_are_applied_to_every_call() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/src\n", "");
        let opts = ChezmoiOptions {
            base_args: vec!["--source".into(), "/src".into()],
            env: vec![("OP_ACCOUNT".into(), "acct".into())],
            ..ChezmoiOptions::default()
        };
        ChezmoiClient::new(fake.clone(), opts).source_dir().unwrap();
        let call = &fake.calls()[0];
        assert_eq!(call.args[0], "--source");
        assert!(
            call.env
                .contains(&("OP_ACCOUNT".to_string(), "acct".to_string()))
        );
    }

    #[test]
    fn re_add_passes_target_with_default_timeout() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .re_add(std::path::Path::new("/Users/x/.zshrc"))
            .unwrap();
        let call = &fake.calls()[0];
        assert_eq!(
            call.args,
            vec!["--no-tty", "--no-pager", "re-add", "/Users/x/.zshrc"]
        );
        assert_eq!(call.timeout, ChezmoiOptions::default().timeout);
    }

    #[test]
    fn update_uses_network_timeout() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone()).update().unwrap();
        let call = &fake.calls()[0];
        assert_eq!(call.args, vec!["--no-tty", "--no-pager", "update"]);
        // update pulls over the network: 120s like git fetch, not the 30s default
        assert_eq!(call.timeout, Duration::from_secs(120));
    }

    #[test]
    fn target_paths_maps_lines() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/Users/x/.zshrc\n/Users/x/.config/a\n", "");
        let out = client(fake)
            .target_paths(&["/s/dot_zshrc".into(), "/s/dot_config/a".into()])
            .unwrap();
        assert_eq!(
            out,
            vec![
                std::path::PathBuf::from("/Users/x/.zshrc"),
                "/Users/x/.config/a".into()
            ]
        );
    }
}
