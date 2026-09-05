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

impl EvalFailureKind {
    /// Short human name for status lines — never raw stderr.
    pub fn label(&self) -> &'static str {
        match self {
            Self::OnePasswordMultipleAccounts => "1Password: multiple accounts",
            Self::OnePasswordAuth => "1Password locked or signed out",
            Self::AgeDecrypt => "age decryption failed",
            Self::GpgDecrypt => "gpg decryption failed",
            Self::TemplateError => "template error",
            Self::Unknown => "chezmoi error",
        }
    }
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
    /// The target is managed, but it is not a regular file — a directory or
    /// an entry type this app doesn't probe. Chezmoi is the authority on
    /// that, and it says so by failing `cat`; a caller walking managed
    /// entries must treat this as "skip", not as breakage.
    #[error("not a regular file: {target}")]
    NotAFile { target: PathBuf },
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("chezmoi exited with code {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("failed to parse chezmoi output ({context}): {detail}")]
    Parse {
        context: &'static str,
        detail: String,
    },
    /// The call itself was wrong — not chezmoi failing, us asking wrongly.
    /// Surfaced rather than asserted because this runs inside a GUI: a panic
    /// would take the window with it, and a silent no-op would be worse than
    /// either.
    #[error("{0}")]
    Failed(String),
}

/// How many times a chezmoi call is attempted when 1Password answers
/// "locked or signed out" — the first try plus two retries.
///
/// Bounded, and short. A vault that is genuinely locked must still fail, and
/// fail SOON: an unbounded retry against a locked 1Password is
/// indistinguishable from the interactive hang `--no-tty` exists to kill.
const ONEPASSWORD_ATTEMPTS: u32 = 3;

/// How long to wait between those attempts. Long enough for `op` to finish
/// coming back (an unlock the user has just answered), short enough that the
/// worst case — 2 × this — stays under the second a person will sit through.
const ONEPASSWORD_BACKOFF: Duration = Duration::from_millis(250);

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

    /// Like [`Self::cat`], but for a target that MIGHT not be a file — the
    /// scanner probes candidates before knowing what they are. The failure is
    /// expected and handled, so it is logged as such instead of as an error.
    pub fn cat_probe(&self, target: &Path) -> Result<Vec<u8>, ChezmoiError> {
        let req = self
            .request(&["cat", &target.to_string_lossy()])
            .failure_is_expected();
        let out = self.runner.run(req)?;
        if out.success() {
            return Ok(out.stdout);
        }
        let stderr = out.stderr_utf8();
        if Self::is_not_a_file_stderr(&stderr) {
            return Err(ChezmoiError::NotAFile {
                target: target.to_path_buf(),
            });
        }
        if let Some(eval) = classify_eval_stderr(&stderr) {
            return Err(ChezmoiError::Eval(eval));
        }
        Err(ChezmoiError::Exit {
            code: out.exit_code,
            stderr,
        })
    }

    /// Every chezmoi call goes through here, and a transient 1Password
    /// failure is retried before it is believed (spec §1.3, §2.7).
    ///
    /// `op` hiccups: the vault is a moment from unlocked, the biometric
    /// prompt has just been answered, the agent is restarting. Chezmoi
    /// surfaces that as an ordinary template failure, so a single drifted
    /// file used to come back "1Password locked or signed out" when trying
    /// again by hand worked immediately. Same shape as `GitClient`'s
    /// index.lock retry (0.1.16): bounded, logged per attempt, and NOT the
    /// place to be clever — three attempts and half a second, then the honest
    /// failure with its remediation hint.
    ///
    /// Only [`EvalFailureKind::OnePasswordAuth`]. `OnePasswordMultipleAccounts`
    /// is a CONFIGURATION error whose fix is a setting, and every other
    /// failure returns on the first attempt as it always has: retrying a
    /// decided answer only delays it.
    ///
    /// WHAT A RETRY CAN REPEAT. Every subcommand reached through here is
    /// idempotent for this purpose with ONE exception, and it is worth naming
    /// rather than discovering: [`Self::update`] (the app's Sync all) is a
    /// whole-tree pull-and-apply, so it RUNS SCRIPTS — a retry can re-run a
    /// `run_` script the failed attempt already ran. That is inside chezmoi's
    /// own contract, and indistinguishable from the user pressing Sync twice,
    /// which `run_once_` / `run_onchange_` scripts already have to survive.
    /// Everything else is a content copy that neither commits nor pushes:
    /// `cat`, `status`, `managed`, `source-path`, `target-path`, `state dump`
    /// and `execute-template` only read, and `apply`/`re-add` rewrite the
    /// same bytes from the same source on a second attempt.
    ///
    /// The retry is also keyed on the ONE failure that says nothing ran:
    /// `OnePasswordAuth` comes from template evaluation, which happens before
    /// chezmoi writes anything for that entry. So an attempt that failed this
    /// way did not produce the source change that would drive chezmoi's own
    /// `autoCommit`/`autoPush`, and a retry cannot double-commit. That is a
    /// property of this specific failure kind, not of retrying in general —
    /// which is the other reason the guard tests the kind and not merely
    /// "some eval failure happened".
    ///
    /// "That entry" is the precise scope, and a BATCHED invocation is why it
    /// has to be said: one call carries many targets, so an attempt that
    /// failed on the fourth entry may already have written the three before
    /// it. The example is [`Self::apply_many`] — `resolve.rs`'s
    /// `resolve_batch` hands it the whole keep-source group, and it renders
    /// entry by entry. NOT [`Self::re_add`], whose batch cannot reach this at
    /// all: `resolve_batch`'s pre-flight refuses a templated keep-disk before
    /// the batch is formed, so a batched `re-add` never carries a target with
    /// a template to evaluate.
    ///
    /// Safety is unchanged, and for the reasons above rather than by luck:
    /// the retry rewrites identical bytes from the same source, and `apply`
    /// never touches the source repo — so the entries already written are
    /// destination files, with nothing for `autoCommit` to have committed and
    /// so nothing for a retry to double-commit.
    fn run_ok_with_timeout(
        &self,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Vec<u8>, ChezmoiError> {
        let mut attempt = 1;
        loop {
            match self.run_ok_once(args, stdin.clone(), timeout) {
                Err(ChezmoiError::Eval(eval))
                    if eval.kind == EvalFailureKind::OnePasswordAuth
                        && attempt < ONEPASSWORD_ATTEMPTS =>
                {
                    // Logged, not silent: a retry nobody can see is
                    // indistinguishable from a hang, and this one costs real
                    // wall-clock. The hint, not the raw stderr — chezmoi's
                    // 1Password stderr quotes the failing `op://` secret
                    // REFERENCE, and the log is a file users paste into
                    // issues.
                    crate::log_info!(
                        "chezmoi",
                        "1Password not ready (attempt {attempt}/{ONEPASSWORD_ATTEMPTS}) for \
                         `chezmoi {}`: retrying in {}ms — {}",
                        args.join(" "),
                        ONEPASSWORD_BACKOFF.as_millis(),
                        eval.hint
                    );
                    attempt += 1;
                    std::thread::sleep(ONEPASSWORD_BACKOFF);
                }
                other => return other,
            }
        }
    }

    /// One invocation, no retry — the body [`Self::run_ok_with_timeout`]
    /// wraps.
    fn run_ok_once(
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
            .map_err(|e| match &e {
                // A managed DIRECTORY (or other non-file entry) — chezmoi
                // reports it by failing `cat`. Typed here so callers can skip
                // it instead of matching on stderr text.
                ChezmoiError::Exit { stderr, .. } if Self::is_not_a_file_stderr(stderr) => {
                    ChezmoiError::NotAFile {
                        target: target.to_path_buf(),
                    }
                }
                _ => e,
            })
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

    /// True when a failed `chezmoi cat` means "this target isn't a file"
    /// rather than a real failure. Chezmoi phrases it exactly this way.
    pub(crate) fn is_not_a_file_stderr(stderr: &str) -> bool {
        stderr.contains("not a file, script, or symlink")
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

    /// Apply EXACTLY the listed destinations, in ONE invocation. Same
    /// per-invocation economics as [`Self::re_add`].
    ///
    /// An empty slice is REFUSED rather than passed through: with no targets
    /// chezmoi applies the ENTIRE source state over the destination, which is
    /// never what a caller with an empty list meant. Use [`Self::apply`] with
    /// `None` to ask for that on purpose.
    pub fn apply_many(&self, targets: &[PathBuf]) -> Result<(), ChezmoiError> {
        if targets.is_empty() {
            return Err(ChezmoiError::Failed(
                "apply_many called with no targets; chezmoi would apply the \
                 ENTIRE source state over the destination, which no caller means"
                    .into(),
            ));
        }
        let mut args = vec!["apply".to_string(), "--force".to_string()];
        args.extend(targets.iter().map(|t| t.to_string_lossy().into_owned()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_ok(&refs, None)?;
        Ok(())
    }

    /// Apply the target (or, with `None`, everything). Passes `--force`: a
    /// drifted destination is by definition "modified since chezmoi last
    /// wrote it", and chezmoi would otherwise prompt `(diff/overwrite/…)?`
    /// — which under `--no-tty` dies with `EOF`. Callers apply only on an
    /// explicit user decision, so overwriting is exactly the contract.
    pub fn apply(&self, target: Option<&Path>) -> Result<(), ChezmoiError> {
        match target {
            Some(t) => self.run_ok(&["apply", "--force", &t.to_string_lossy()], None)?,
            None => self.run_ok(&["apply", "--force"], None)?,
        };
        Ok(())
    }

    /// Re-add EXACTLY the listed destinations into the source, in ONE
    /// invocation. An empty slice is REFUSED, for the same reason
    /// [`Self::apply_many`] refuses one: with no targets chezmoi re-adds
    /// EVERY modified file.
    ///
    /// NOTE: chezmoi silently ignores templated sources — callers must
    /// pre-detect `.tmpl` and refuse.
    ///
    /// chezmoi takes `target...` and re-adds every one, which matters far
    /// more than it looks: a single `re-add` costs ~2.5s on a real setup
    /// (process start plus config and template evaluation), and that cost is
    /// per INVOCATION, not per file. Resolving thirty files one call at a
    /// time paid it thirty times. An empty slice keeps chezmoi's own meaning
    /// — every modified file — so callers must not hand one over by accident.
    pub fn re_add(&self, targets: &[PathBuf]) -> Result<(), ChezmoiError> {
        if targets.is_empty() {
            return Err(ChezmoiError::Failed(
                "re_add called with no targets; chezmoi would re-add EVERY \
                 modified file, which no caller means"
                    .into(),
            ));
        }
        let mut args = vec!["re-add".to_string()];
        args.extend(targets.iter().map(|t| t.to_string_lossy().into_owned()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_ok(&refs, None)?;
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

    /// A templated file rendered through `onepasswordRead` fails when `op`
    /// hiccups, and retrying by hand works (spec §1.3) — so retry by machine,
    /// bounded and logged, the way `GitClient` treats index.lock contention.
    ///
    /// MUTATION: drop the retry loop from `run_ok_with_timeout` (return the
    /// first `Err`) → the `cat` fails and never sees the second response.
    #[test]
    fn a_transient_1password_failure_is_retried() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(1, "", "chezmoi: template: x.tmpl:1:1: executing at <onepasswordRead>: error calling onepasswordRead: op signin --raw: exit status 1\n");
        fake.push_ok(0, "rendered\n", "");
        let out = client(fake.clone())
            .cat(Path::new("/Users/x/.zshrc"))
            .unwrap();
        assert_eq!(out, b"rendered\n");
        assert_eq!(fake.calls().len(), 2, "retried once, then succeeded");
    }

    /// Multiple-accounts is a CONFIGURATION error, not a hiccup: retrying it
    /// only delays the real hint ("select an account in Settings").
    ///
    /// MUTATION: key the retry on `classify_eval_stderr(..).is_some()` rather
    /// than on `OnePasswordAuth` → three calls instead of one, and the user
    /// waits half a second for an answer that was already known.
    #[test]
    fn a_multiple_accounts_failure_is_not_retried() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(1, "", "[ERROR] multiple accounts found. Use the --account flag or set the OP_ACCOUNT environment variable\n");
        let err = client(fake.clone())
            .cat(Path::new("/Users/x/.zshrc"))
            .unwrap_err();
        assert!(matches!(
            err,
            ChezmoiError::Eval(EvalFailure {
                kind: EvalFailureKind::OnePasswordMultipleAccounts,
                ..
            })
        ));
        assert_eq!(fake.calls().len(), 1, "no retry");
    }

    /// BOUNDED. A 1Password that is locked and stays locked must still fail,
    /// and fail soon: an unbounded retry is indistinguishable from a hang,
    /// which is exactly the failure mode `--no-tty` was added to kill.
    ///
    /// MUTATION: drop the `attempt < ONEPASSWORD_ATTEMPTS` bound → the loop
    /// eats all eight queued responses (and then panics on the ninth).
    #[test]
    fn a_persistent_1password_failure_still_fails_after_the_bounded_retries() {
        let fake = Arc::new(FakeRunner::new());
        for _ in 0..8 {
            fake.push_ok(
                1,
                "",
                "error calling onepasswordRead: op signin --raw: exit status 1\n",
            );
        }
        let err = client(fake.clone())
            .cat(Path::new("/Users/x/.zshrc"))
            .unwrap_err();
        assert!(matches!(
            err,
            ChezmoiError::Eval(EvalFailure {
                kind: EvalFailureKind::OnePasswordAuth,
                ..
            })
        ));
        assert!(
            fake.calls().len() <= 4,
            "bounded: {} attempts",
            fake.calls().len()
        );
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
    fn apply_passes_force_to_survive_modified_destinations() {
        // Without --force, applying over an externally modified destination
        // prompts "(diff/overwrite/…)?" and dies with EOF under --no-tty.
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        fake.push_ok(0, "", "");
        let c = client(fake.clone());
        c.apply(Some(std::path::Path::new("/Users/x/.zshrc")))
            .unwrap();
        c.apply(None).unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[0].args,
            vec![
                "--no-tty",
                "--no-pager",
                "apply",
                "--force",
                "/Users/x/.zshrc"
            ]
        );
        assert_eq!(
            calls[1].args,
            vec!["--no-tty", "--no-pager", "apply", "--force"]
        );
    }

    #[test]
    fn re_add_passes_target_with_default_timeout() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .re_add(&[PathBuf::from("/Users/x/.zshrc")])
            .unwrap();
        let call = &fake.calls()[0];
        assert_eq!(
            call.args,
            vec!["--no-tty", "--no-pager", "re-add", "/Users/x/.zshrc"]
        );
        assert_eq!(call.timeout, ChezmoiOptions::default().timeout);
    }

    /// The whole point of the multi-target form: N files, ONE invocation.
    /// chezmoi's ~2.5s startup is paid per CALL, so this is the difference
    /// between 2.5s and 2.5s×N for a batch of decisions.
    #[test]
    fn re_add_sends_every_target_in_a_single_invocation() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .re_add(&[
                PathBuf::from("/Users/x/.zshrc"),
                PathBuf::from("/Users/x/.gitconfig"),
                PathBuf::from("/Users/x/.vimrc"),
            ])
            .unwrap();
        assert_eq!(fake.calls().len(), 1, "one invocation, not three");
        assert_eq!(
            fake.calls()[0].args,
            vec![
                "--no-tty",
                "--no-pager",
                "re-add",
                "/Users/x/.zshrc",
                "/Users/x/.gitconfig",
                "/Users/x/.vimrc"
            ]
        );
    }

    /// An empty slice means "every modified file" to chezmoi, which would
    /// sweep unrelated drift into the source. No caller means that.
    #[test]
    fn re_add_refuses_an_empty_target_list_rather_than_re_adding_everything() {
        let fake = Arc::new(FakeRunner::new());
        let err = client(fake.clone()).re_add(&[]).unwrap_err();
        assert!(
            err.to_string().contains("no targets"),
            "must say why it refused: {err}"
        );
        assert!(
            fake.calls().is_empty(),
            "and must not have invoked chezmoi at all"
        );
    }

    #[test]
    fn apply_many_refuses_an_empty_target_list_rather_than_applying_everything() {
        let fake = Arc::new(FakeRunner::new());
        let err = client(fake.clone()).apply_many(&[]).unwrap_err();
        assert!(
            err.to_string().contains("no targets"),
            "must say why it refused: {err}"
        );
        assert!(
            fake.calls().is_empty(),
            "and must not have invoked chezmoi at all"
        );
    }

    #[test]
    fn apply_many_sends_every_target_in_a_single_invocation() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .apply_many(&[PathBuf::from("/Users/x/.a"), PathBuf::from("/Users/x/.b")])
            .unwrap();
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(
            fake.calls()[0].args,
            vec![
                "--no-tty",
                "--no-pager",
                "apply",
                "--force",
                "/Users/x/.a",
                "/Users/x/.b"
            ]
        );
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
