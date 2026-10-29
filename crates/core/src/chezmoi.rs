//! ChezmoiClient + parsers + error classification.

use std::collections::{BTreeSet, HashMap};
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
    /// The folder holding a derived config this client reads
    /// ([`ChezmoiClient::without_git_automation`]); removed when the last
    /// clone drops.
    _derived_config: Option<Arc<tempfile::TempDir>>,
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

/// The managed folders and links, from `managed --include dirs|symlinks`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagedKinds {
    pub folders: BTreeSet<PathBuf>,
    pub links: BTreeSet<PathBuf>,
}

/// Turn chezmoi's git automation off in a `dump-config` JSON object, and pin
/// its persistent state to `state` unless the config already names one.
/// `dump-config` spells the git keys in lower case (`autocommit`) while the
/// docs use camel case; chezmoi reads either, so an existing key is set in
/// whatever case it has and a missing one is added.
fn git_automation_off(config: &mut serde_json::Value, state: &Path) -> Result<(), ChezmoiError> {
    let not_an_object = || ChezmoiError::Parse {
        context: "dump-config",
        detail: "expected a JSON object with a `git` object".into(),
    };
    let root = config.as_object_mut().ok_or_else(not_an_object)?;
    let pinned = root
        .get("persistentState")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|path| !path.is_empty());
    if !pinned {
        root.insert(
            "persistentState".into(),
            serde_json::Value::String(state.to_string_lossy().into_owned()),
        );
    }
    let git = root
        .entry("git")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(not_an_object)?;
    for wanted in ["autoAdd", "autoCommit", "autoPush"] {
        let key = git
            .keys()
            .find(|key| key.eq_ignore_ascii_case(wanted))
            .cloned()
            .unwrap_or_else(|| wanted.to_string());
        git.insert(key, serde_json::Value::Bool(false));
    }
    Ok(())
}

/// Write `config` to `chezmoi.json` in a new temporary folder only the user
/// can open (0700, the file 0600): it is a copy of the user's whole chezmoi
/// config, template data and secret-manager settings included, and the
/// temporary folder may be the shared `/tmp`.
fn write_private_config(
    config: &serde_json::Value,
) -> Result<(tempfile::TempDir, PathBuf), ChezmoiError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let written = |e: std::io::Error| {
        ChezmoiError::Failed(format!(
            "could not write a config without git automation: {e}"
        ))
    };
    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(written)?;
    let path = dir.path().join("chezmoi.json");
    let bytes = serde_json::to_vec(config).map_err(|e| ChezmoiError::Failed(e.to_string()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(written)?;
    Ok((dir, path))
}

/// `base_args` reading `config` (JSON) instead of any config file they named.
fn with_config_file(base_args: &[String], config: &Path) -> Vec<String> {
    let mut args = Vec::with_capacity(base_args.len() + 4);
    let mut iter = base_args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" | "--config-format" => {
                iter.next();
            }
            _ if arg.starts_with("--config=") || arg.starts_with("--config-format=") => {}
            _ => args.push(arg.clone()),
        }
    }
    args.extend([
        "--config".to_string(),
        config.to_string_lossy().into_owned(),
        "--config-format".to_string(),
        "json".to_string(),
    ]);
    args
}

fn parse_status(text: &str) -> Result<Vec<StatusEntry>, ChezmoiError> {
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

/// How many times a chezmoi call is attempted when another chezmoi holds its
/// persistent-state (Bolt) lock. `chezmoi apply` holds it for its whole run,
/// and a daemon scan runs WITHOUT the daemon's core lock (spec 2026-10-11),
/// so a scan's `state dump` / `source-path` / `status` routinely meets the
/// app's apply. chezmoi itself waits about a second before giving up, so 40
/// attempts with [`STATE_LOCK_BACKOFF`] between them outlast a bulk apply of
/// a minute — and stay bounded, so a chezmoi that never lets go still fails.
const STATE_LOCK_ATTEMPTS: u32 = 40;

/// The wait between [`STATE_LOCK_ATTEMPTS`].
const STATE_LOCK_BACKOFF: Duration = Duration::from_millis(500);

/// True when chezmoi failed because another chezmoi held its persistent-state
/// lock (chezmoi v2.70.4: `timeout obtaining persistent state lock, is
/// another instance of chezmoi running?`). Nothing ran: the lock is taken
/// before any work, so the call is safe to repeat.
pub(crate) fn is_state_lock_timeout(stderr: &str) -> bool {
    stderr.contains("timeout obtaining persistent state lock")
}

/// The one retry policy for the state lock, shared by every loop that meets
/// it. Called after attempt number `attempt` failed on the lock: returns
/// `true` (after logging on attempts 1, 10, 20 and 30 and sleeping one
/// backoff) when the caller should try again, `false` (after a warning) when
/// the budget is spent and the caller must return the error.
fn state_lock_should_retry(what: &str, attempt: u32) -> bool {
    if attempt >= STATE_LOCK_ATTEMPTS {
        crate::log_warn!(
            "chezmoi",
            "`chezmoi {what}`: another chezmoi still held the persistent-state lock after \
             {attempt} attempts; giving up"
        );
        return false;
    }
    if attempt == 1 || attempt.is_multiple_of(10) {
        crate::log_info!(
            "chezmoi",
            "`chezmoi {what}`: another chezmoi holds the persistent-state lock (attempt \
             {attempt}/{STATE_LOCK_ATTEMPTS}); retrying every {}ms",
            STATE_LOCK_BACKOFF.as_millis()
        );
    }
    std::thread::sleep(STATE_LOCK_BACKOFF);
    true
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
        Self {
            runner,
            opts,
            _derived_config: None,
        }
    }

    /// This client with chezmoi's git automation off: same runner, binary,
    /// environment and flags, but reading a copy of this client's config
    /// (`dump-config`) with `git.autoAdd`, `autoCommit` and `autoPush` set
    /// to false, so `re-add`, `add` and `forget` change the source and leave
    /// committing and pushing to the caller. chezmoi has no per-invocation
    /// switch for this.
    ///
    /// The copy pins `persistentState` to the file the original config uses.
    /// chezmoi otherwise keeps its state next to the config file it read,
    /// and a `re-add` would record what it wrote in a throwaway database the
    /// next `status` never reads.
    pub fn without_git_automation(&self) -> Result<ChezmoiClient, ChezmoiError> {
        let dumped = self.run_ok(&["dump-config", "--format", "json"], None)?;
        let mut config: serde_json::Value =
            serde_json::from_slice(&dumped).map_err(|e| ChezmoiError::Parse {
                context: "dump-config",
                detail: e.to_string(),
            })?;
        let config_file = self.run_utf8(&["execute-template", "{{ .chezmoi.configFile }}"])?;
        let state = Path::new(config_file.trim())
            .parent()
            .map(|dir| dir.join("chezmoistate.boltdb"))
            .ok_or_else(|| {
                ChezmoiError::Failed(format!(
                    "chezmoi named no folder for its config file: {config_file:?}"
                ))
            })?;
        git_automation_off(&mut config, &state)?;
        let (dir, path) = write_private_config(&config)?;
        let mut opts = self.opts.clone();
        opts.base_args = with_config_file(&self.opts.base_args, &path);
        Ok(ChezmoiClient {
            runner: self.runner.clone(),
            opts,
            _derived_config: Some(Arc::new(dir)),
        })
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
        let mut lock_attempt = 1;
        loop {
            match self.run_ok_once(args, stdin.clone(), timeout) {
                // Another chezmoi (the app's `apply`) holds the state lock:
                // nothing ran, so wait it out under the shared policy.
                Err(ChezmoiError::Exit { ref stderr, .. })
                    if is_state_lock_timeout(stderr)
                        && state_lock_should_retry(&args.join(" "), lock_attempt) =>
                {
                    lock_attempt += 1;
                }
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
        parse_status(&text)
    }

    /// `status` for the listed targets only, without descending into a
    /// listed directory. An empty slice is REFUSED: with no targets chezmoi
    /// reports every file, which a scoped caller never means.
    pub fn status_of(&self, targets: &[PathBuf]) -> Result<Vec<StatusEntry>, ChezmoiError> {
        if targets.is_empty() {
            return Err(ChezmoiError::Failed(
                "status_of called with no targets".into(),
            ));
        }
        let mut args = vec![
            "status".to_string(),
            "--path-style=absolute".to_string(),
            "--recursive=false".to_string(),
        ];
        args.extend(targets.iter().map(|t| t.to_string_lossy().into_owned()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        parse_status(&String::from_utf8_lossy(&self.run_ok(&refs, None)?))
    }

    /// The managed folders and links, as the source says (not the disk).
    pub fn managed_kinds(&self) -> Result<ManagedKinds, ChezmoiError> {
        let list = |kind: &str| -> Result<BTreeSet<PathBuf>, ChezmoiError> {
            Ok(self
                .run_utf8(&["managed", "--path-style=absolute", "--include", kind])?
                .lines()
                .map(PathBuf::from)
                .collect())
        };
        Ok(ManagedKinds {
            folders: list("dirs")?,
            links: list("symlinks")?,
        })
    }

    /// What the source says the link at `target` points to: `cat` output with
    /// the one trailing newline chezmoi adds removed. A templated link that
    /// fails to render keeps its `Eval` error.
    pub fn link_text(&self, target: &Path) -> Result<String, ChezmoiError> {
        let bytes = self.run_ok(&["cat", &target.to_string_lossy()], None)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(text.strip_suffix('\n').map(str::to_string).unwrap_or(text))
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

    /// `chezmoi source-path t1 t2 …` for MANY targets in one invocation: one
    /// line per target, in argument order (verified against chezmoi on
    /// 2026-10-11). Chunked so an argv stays a few thousand paths at most.
    /// A chunk that exits non-zero (one unmanaged target fails the whole
    /// call) or answers the wrong number of lines is retried one target at
    /// a time — two lists of different lengths are never zipped. A chunk
    /// that failed because another chezmoi held the persistent-state lock is
    /// retried AS A CHUNK under the state-lock policy: splitting it would
    /// only send every target into the same lock.
    pub fn source_paths(&self, targets: &[PathBuf]) -> Result<Vec<Option<PathBuf>>, ChezmoiError> {
        const CHUNK: usize = 128;
        let mut out = Vec::with_capacity(targets.len());
        for chunk in targets.chunks(CHUNK) {
            let mut args: Vec<String> = vec!["source-path".into()];
            args.extend(chunk.iter().map(|p| p.to_string_lossy().into_owned()));
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let what = format!("source-path ({} targets)", chunk.len());
            let mut lock_attempt = 1;
            let reply = loop {
                // Expected to fail when the chunk holds an unmanaged target:
                // logged as such, not as an error, like `cat_probe`.
                let req = self.request(&arg_refs).failure_is_expected();
                let reply = self.runner.run(req)?;
                if !reply.success() {
                    let stderr = reply.stderr_utf8();
                    if is_state_lock_timeout(&stderr) {
                        if state_lock_should_retry(&what, lock_attempt) {
                            lock_attempt += 1;
                            continue;
                        }
                        // Budget spent: splitting the chunk would only send
                        // every target into the same lock. Fail the call.
                        return Err(ChezmoiError::Exit {
                            code: reply.exit_code,
                            stderr,
                        });
                    }
                }
                break reply;
            };
            let lines: Vec<PathBuf> = if reply.success() {
                String::from_utf8_lossy(&reply.stdout)
                    .lines()
                    .map(|l| PathBuf::from(l.trim()))
                    .collect()
            } else {
                Vec::new()
            };
            if reply.success() && lines.len() == chunk.len() {
                out.extend(lines.into_iter().map(Some));
            } else {
                crate::log_info!(
                    "chezmoi",
                    "source-path for a chunk of {} targets {}; asking one target at a time",
                    chunk.len(),
                    if reply.success() {
                        format!("answered {} lines", lines.len())
                    } else {
                        format!("exited {}", reply.exit_code)
                    }
                );
                for target in chunk {
                    match self.source_path(target) {
                        Ok(p) => out.push(Some(p)),
                        Err(e) => {
                            crate::log_warn!(
                                "chezmoi",
                                "source-path for {} failed ({e}); git signals for it default to \
                                 \"not ahead\" this scan",
                                target.display()
                            );
                            out.push(None);
                        }
                    }
                }
            }
        }
        Ok(out)
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
        let args = targeted_apply_args(targets);
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
            Some(t) => {
                let args = targeted_apply_args(&[t.to_path_buf()]);
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.run_ok(&refs, None)?
            }
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

    /// Record the listed targets as they are on disk, in ONE invocation.
    /// Unlike `re_add` this takes a target whose type changed (a folder that
    /// became a link). `--force` skips the confirmation prompt. An empty
    /// slice is REFUSED, like `forget`.
    pub fn add(&self, targets: &[PathBuf]) -> Result<(), ChezmoiError> {
        if targets.is_empty() {
            return Err(ChezmoiError::Failed("add called with no targets".into()));
        }
        let mut args = vec!["add".to_string(), "--force".to_string()];
        args.extend(targets.iter().map(|t| t.to_string_lossy().into_owned()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_ok(&refs, None)?;
        Ok(())
    }

    /// Add `target` into ANOTHER source directory, `scratch_source`, under an
    /// empty `scratch_config`, and return the source path chezmoi wrote for
    /// it there. The destination is this client's own. With an empty config
    /// there is no autoCommit and no autoPush: the real source repo is never
    /// touched, which is the point mid-merge, where chezmoi's own commit would
    /// sweep up every other conflicted file. The config must not sit in a
    /// folder that holds `target`: chezmoi refuses to add from the config's
    /// folder ("is protected").
    pub fn add_into_source(
        &self,
        target: &Path,
        scratch_source: &Path,
        scratch_config: &Path,
    ) -> Result<PathBuf, ChezmoiError> {
        let destination = self.run_utf8(&["target-path"])?.trim().to_string();
        let source = scratch_source.to_string_lossy();
        let config = scratch_config.to_string_lossy();
        let target = target.to_string_lossy();
        let scoped = |command: &[&str]| -> Vec<String> {
            let mut args: Vec<String> = [
                "--source",
                &source,
                "--config",
                &config,
                "--destination",
                &destination,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            args.extend(command.iter().map(|s| s.to_string()));
            args
        };
        let add = scoped(&["add", "--force", &target]);
        self.run_ok(&add.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
        let path = scoped(&["source-path", &target]);
        let out = self.run_ok(&path.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
        Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
    }

    /// The managed entries strictly under `folder`, as the source says,
    /// absolute. `include` narrows the kinds (`"files"`); `None` lists every
    /// kind, folders and links included.
    pub fn managed_under(
        &self,
        folder: &Path,
        include: Option<&str>,
    ) -> Result<Vec<PathBuf>, ChezmoiError> {
        let folder_arg = folder.to_string_lossy();
        let mut args = vec!["managed", "--path-style=absolute"];
        if let Some(kind) = include {
            args.extend(["--include", kind]);
        }
        args.push(&folder_arg);
        Ok(self
            .run_utf8(&args)?
            .lines()
            .map(PathBuf::from)
            .filter(|p| p != folder && p.starts_with(folder))
            .collect())
    }

    /// Remove the listed targets from the source state, in ONE invocation:
    /// chezmoi deletes their source files (and, with autoCommit, commits the
    /// removal). `--force` skips chezmoi's confirmation prompt, which under
    /// `--no-tty` would die with `EOF`. An empty slice is REFUSED: there is
    /// no "every file" meaning to fall back on, only a caller bug.
    pub fn forget(&self, targets: &[PathBuf]) -> Result<(), ChezmoiError> {
        if targets.is_empty() {
            return Err(ChezmoiError::Failed("forget called with no targets".into()));
        }
        let mut args = vec!["forget".to_string(), "--force".to_string()];
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

/// `apply --force <targets>`, with every missing ancestor directory of a
/// target listed first. chezmoi refuses to apply a file whose parent
/// directory is missing (`stat <dir>: no such file or directory`) and fails
/// the whole invocation, so a deleted folder made Keep source fail for every
/// file in the batch. The ancestors are managed directories (a target's
/// parent exists in the source state), and `--recursive=false` makes chezmoi
/// create each one without applying the rest of its contents. Targets whose
/// parents all exist get the plain argument list.
fn targeted_apply_args(targets: &[PathBuf]) -> Vec<String> {
    let mut missing = std::collections::BTreeSet::new();
    for target in targets {
        for dir in target.ancestors().skip(1) {
            if dir.as_os_str().is_empty() || dir.exists() {
                break;
            }
            missing.insert(dir.to_path_buf());
        }
    }
    let mut args = vec!["apply".to_string(), "--force".to_string()];
    if !missing.is_empty() {
        args.push("--recursive=false".to_string());
    }
    // BTreeSet order puts a parent before its children.
    args.extend(missing.iter().map(|d| d.to_string_lossy().into_owned()));
    args.extend(targets.iter().map(|t| t.to_string_lossy().into_owned()));
    args
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
        c.apply(Some(std::path::Path::new("/.zshrc"))).unwrap();
        c.apply(None).unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[0].args,
            vec!["--no-tty", "--no-pager", "apply", "--force", "/.zshrc"]
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

    /// MUTATION: skip the `dir.exists()` check → the existing parent is
    /// listed too and the assertion on the exact args fails; drop the
    /// ancestor walk → `--recursive=false` and both directories vanish.
    #[test]
    fn apply_many_lists_missing_parent_directories_first() {
        let home = tempfile::tempdir().unwrap();
        let gone = home.path().join(".d");
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .apply_many(&[
                gone.join("sub/deep"),
                gone.join("top"),
                home.path().join(".a"),
            ])
            .unwrap();
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        assert_eq!(
            fake.calls()[0].args,
            vec![
                "--no-tty".to_string(),
                "--no-pager".into(),
                "apply".into(),
                "--force".into(),
                "--recursive=false".into(),
                s(gone.clone()),
                s(gone.join("sub")),
                s(gone.join("sub/deep")),
                s(gone.join("top")),
                s(home.path().join(".a")),
            ]
        );
    }

    #[test]
    fn apply_many_sends_every_target_in_a_single_invocation() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .apply_many(&[PathBuf::from("/.a"), PathBuf::from("/.b")])
            .unwrap();
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(
            fake.calls()[0].args,
            vec!["--no-tty", "--no-pager", "apply", "--force", "/.a", "/.b"]
        );
    }

    /// MUTATION: drop `--force` → the args differ (and in the field chezmoi
    /// would prompt and die on EOF); drop the empty check → no error.
    #[test]
    fn forget_forces_and_refuses_an_empty_list() {
        let fake = Arc::new(FakeRunner::new());
        assert!(client(fake.clone()).forget(&[]).is_err());
        assert!(fake.calls().is_empty());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .forget(&[PathBuf::from("/.a")])
            .unwrap();
        assert_eq!(
            fake.calls()[0].args,
            vec!["--no-tty", "--no-pager", "forget", "--force", "/.a"]
        );
    }

    /// MUTATION: drop `--force` → the args differ (chezmoi would prompt and
    /// die on EOF); drop the empty check → no error.
    #[test]
    fn add_forces_and_refuses_an_empty_list() {
        let fake = Arc::new(FakeRunner::new());
        assert!(client(fake.clone()).add(&[]).is_err());
        assert!(fake.calls().is_empty());
        fake.push_ok(0, "", "");
        client(fake.clone())
            .add(&[PathBuf::from("/.a"), PathBuf::from("/.b")])
            .unwrap();
        assert_eq!(
            fake.calls()[0].args,
            vec!["--no-tty", "--no-pager", "add", "--force", "/.a", "/.b"]
        );
    }

    /// MUTATION: drop the scratch `--config` from `add_into_source` → the
    /// args differ (chezmoi would autoCommit into the real repo); return the
    /// destination instead of `source-path`'s answer → the path differs.
    #[test]
    fn add_into_source_scopes_add_to_the_scratch_source() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/home\n", "");
        fake.push_ok(0, "", "");
        fake.push_ok(0, "/tmp/s/dot_d/symlink_h\n", "");
        let leaf = client(fake.clone())
            .add_into_source(
                Path::new("/home/.d/h"),
                Path::new("/tmp/s"),
                Path::new("/tmp/c/chezmoi.toml"),
            )
            .unwrap();
        assert_eq!(leaf, PathBuf::from("/tmp/s/dot_d/symlink_h"));
        let calls = fake.calls();
        assert_eq!(calls[0].args, vec!["--no-tty", "--no-pager", "target-path"]);
        let scoped = |command: &[&str]| -> Vec<String> {
            let mut args: Vec<String> = [
                "--no-tty",
                "--no-pager",
                "--source",
                "/tmp/s",
                "--config",
                "/tmp/c/chezmoi.toml",
                "--destination",
                "/home",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            args.extend(command.iter().map(|s| s.to_string()));
            args
        };
        assert_eq!(calls[1].args, scoped(&["add", "--force", "/home/.d/h"]));
        assert_eq!(calls[2].args, scoped(&["source-path", "/home/.d/h"]));
    }

    /// MUTATION: drop the `p != folder` filter from `managed_under` → the
    /// folder itself is listed.
    #[test]
    fn managed_under_lists_only_what_is_inside() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/h/.d\n/h/.d/a\n/h/.d/sub/b\n", "");
        let got = client(fake.clone())
            .managed_under(Path::new("/h/.d"), Some("files"))
            .unwrap();
        assert_eq!(
            got,
            vec![PathBuf::from("/h/.d/a"), PathBuf::from("/h/.d/sub/b")]
        );
        assert_eq!(
            fake.calls()[0].args,
            vec![
                "--no-tty",
                "--no-pager",
                "managed",
                "--path-style=absolute",
                "--include",
                "files",
                "/h/.d"
            ]
        );
    }

    /// MUTATION: drop `--recursive=false` from `status_of` → the args differ
    /// (and chezmoi would list every file under a listed folder); drop the
    /// empty check → no error.
    #[test]
    fn status_of_is_not_recursive() {
        let fake = Arc::new(FakeRunner::new());
        assert!(client(fake.clone()).status_of(&[]).is_err());
        assert!(fake.calls().is_empty());
        fake.push_ok(0, " M /h/.a\n", "");
        let got = client(fake.clone())
            .status_of(&[PathBuf::from("/h/.a")])
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, PathBuf::from("/h/.a"));
        assert_eq!(
            fake.calls()[0].args,
            vec![
                "--no-tty",
                "--no-pager",
                "status",
                "--path-style=absolute",
                "--recursive=false",
                "/h/.a"
            ]
        );
    }

    /// MUTATION: `trim()` the whole reply instead of stripping one `\n` → the
    /// link text `target  ` (trailing spaces are part of it) loses them.
    #[test]
    fn link_text_strips_one_newline() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "../x y  \n\n", "");
        let text = client(fake.clone()).link_text(Path::new("/h/.l")).unwrap();
        assert_eq!(text, "../x y  \n");
        assert_eq!(
            fake.calls()[0].args,
            vec!["--no-tty", "--no-pager", "cat", "/h/.l"]
        );
    }

    /// MUTATION: swap the two `--include` values in `managed_kinds` → folders
    /// and links trade places.
    #[test]
    fn managed_kinds_reads_dirs_and_symlinks() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/h/.d\n/h/.e\n", "");
        fake.push_ok(0, "/h/.l\n", "");
        let kinds = client(fake.clone()).managed_kinds().unwrap();
        assert_eq!(
            kinds.folders,
            BTreeSet::from([PathBuf::from("/h/.d"), PathBuf::from("/h/.e")])
        );
        assert_eq!(kinds.links, BTreeSet::from([PathBuf::from("/h/.l")]));
        let calls = fake.calls();
        assert_eq!(
            calls[0].args,
            vec![
                "--no-tty",
                "--no-pager",
                "managed",
                "--path-style=absolute",
                "--include",
                "dirs"
            ]
        );
        assert_eq!(calls[1].args[5], "symlinks");
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

    /// A chunk that exits 0 but answers fewer lines than it has targets is
    /// retried one target at a time, never zipped against the short reply.
    ///
    /// MUTATION: drop the `lines.len() == chunk.len()` check from
    /// `source_paths` → the result has one entry for two targets.
    #[test]
    fn source_paths_retries_a_short_reply_per_target() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/src/dot_a\n", ""); // batch: one line for two targets
        fake.push_ok(0, "/src/dot_a\n", ""); // retry, first target
        fake.push_ok(1, "", "chezmoi: not managed"); // retry, second target
        let got = client(fake.clone())
            .source_paths(&["/h/.a".into(), "/h/.b".into()])
            .unwrap();
        assert_eq!(got, vec![Some(PathBuf::from("/src/dot_a")), None]);
        assert_eq!(fake.calls().len(), 3);
    }

    /// chezmoi v2.70.4's stderr when another chezmoi (the app's `apply`)
    /// holds the persistent-state lock for longer than chezmoi waits.
    const STATE_LOCK_STDERR: &str = "chezmoi: timeout obtaining persistent state lock, is another instance of chezmoi running?\n";

    /// A scan's `state dump` that meets the app's `apply` waits it out
    /// instead of killing the whole scan (final review finding 2a).
    ///
    /// MUTATION: drop the `is_state_lock_timeout` arm from
    /// `run_ok_with_timeout` → the first reply is returned as `Err(Exit)`.
    #[test]
    fn a_state_lock_timeout_is_retried() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(1, "", STATE_LOCK_STDERR);
        fake.push_ok(0, "/src\n", "");
        let got = client(fake.clone()).source_dir().unwrap();
        assert_eq!(got, PathBuf::from("/src"));
        assert_eq!(fake.calls().len(), 2, "retried once, then succeeded");
    }

    /// A batched `source-path` chunk that meets the state lock is retried AS
    /// A CHUNK; splitting it per target would send each target into the
    /// same lock and degrade conflicts to "not ahead" (finding 2b).
    ///
    /// MUTATION: treat the lock timeout like any other chunk failure (drop
    /// the lock branch in `source_paths`) → the chunk falls back per target:
    /// three calls, and the per-target replies are not what was scripted.
    #[test]
    fn source_paths_retries_a_state_locked_chunk_whole() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(1, "", STATE_LOCK_STDERR);
        fake.push_ok(0, "/src/dot_a\n/src/dot_b\n", "");
        let got = client(fake.clone())
            .source_paths(&["/h/.a".into(), "/h/.b".into()])
            .unwrap();
        assert_eq!(
            got,
            vec![
                Some(PathBuf::from("/src/dot_a")),
                Some(PathBuf::from("/src/dot_b"))
            ]
        );
        assert_eq!(fake.calls().len(), 2, "one retry of the whole chunk");
    }

    /// `dump-config`'s lower-case git keys are switched off in place, a
    /// missing one is added, and the state database is pinned only when the
    /// config named none.
    ///
    /// MUTATION: match the git keys case-sensitively in `git_automation_off`
    /// → a camel-case `autoCommit` is added beside `autocommit: true`.
    #[test]
    fn git_automation_off_switches_the_dumped_keys_and_pins_the_state() {
        let mut config = serde_json::json!({
            "persistentState": "",
            "git": { "autocommit": true, "autopush": true, "command": "git" },
        });
        git_automation_off(&mut config, Path::new("/cfg/chezmoistate.boltdb")).unwrap();
        assert_eq!(
            config,
            serde_json::json!({
                "persistentState": "/cfg/chezmoistate.boltdb",
                "git": {
                    "autocommit": false,
                    "autopush": false,
                    "autoAdd": false,
                    "command": "git",
                },
            })
        );

        let mut own = serde_json::json!({ "persistentState": "/own.boltdb" });
        git_automation_off(&mut own, Path::new("/cfg/chezmoistate.boltdb")).unwrap();
        assert_eq!(own["persistentState"], "/own.boltdb");
    }

    /// The derived config replaces any config flag, in either spelling.
    ///
    /// MUTATION: keep `--config <path>` in `with_config_file` → chezmoi
    /// gets two config files and the args differ.
    #[test]
    fn with_config_file_replaces_the_config_flags() {
        let base: Vec<String> = [
            "--source",
            "/s",
            "--config",
            "/c.toml",
            "--config-format=toml",
            "--no-tty",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            with_config_file(&base, Path::new("/t/chezmoi.json")),
            [
                "--source",
                "/s",
                "--no-tty",
                "--config",
                "/t/chezmoi.json",
                "--config-format",
                "json",
            ]
            .map(String::from)
            .to_vec()
        );
    }

    /// The derived config copies the user's whole chezmoi config, so only
    /// the user may read it: folder 0700, file 0600.
    ///
    /// MUTATION: create the folder with `tempfile::tempdir()` in
    /// `write_private_config` → it is 0755 under the usual umask.
    #[test]
    fn the_derived_config_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = write_private_config(&serde_json::json!({ "data": {} })).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.path()), 0o700, "the folder");
        assert_eq!(mode(&path), 0o600, "the file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"data":{}}"#);
    }
}
