# chezmoi-ui v0 — Plan 1: Foundation & Drift Model

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cargo workspace + `czui-core` crate: a subprocess seam, chezmoi/git CLI wrappers, the four-state drift classifier, a composed drift scanner, and a runnable `drift-scan` debug binary that classifies real dotfile drift on this machine.

**Architecture:** Everything chezmoi-semantic goes through the chezmoi CLI; git through the user's git binary. A `CommandRunner` trait is the single subprocess seam so every wrapper is testable with a fake. The classifier is pure (hashes in, `DriftClass` out); the scanner composes wrappers + classifier and degrades gracefully when templates fail to render (1Password etc.).

**Tech Stack:** Rust stable (edition 2024), thiserror, serde/serde_json, sha2, wait-timeout, tempfile (dev). No async runtime — std threads only.

**Plan series (spec: `docs/superpowers/specs/2026-07-19-chezmoi-ui-v0-design.md`):**
1. **This plan** — foundation & drift model (spec §3.4 core, §4, parts of §10/§12)
2. Merge engine & template span mapping (§6)
3. Journal, proto & daemon (§3.1, §3.3, §8)
4. GPUI app shell: menubar, dashboard, review, settings (§3.2, §7, §9)
5. Merge editor UI, resolution sessions, sync pipeline, packaging, E2E (§5, §6.3, §7.3, §11)

## Global Constraints

- macOS only; chezmoi ≥ 2.70; user's own `git` binary; gpui = 0.2.2 exact (later plans).
- chezmoi is the single source of truth for chezmoi semantics — never reimplement templating/ignore/encryption logic.
- Every subprocess: explicit timeout, captured stderr, non-interactive (no hidden prompts — spec §9).
- No async runtime anywhere in v0; std threads.
- Library crates: `thiserror` errors, no `anyhow`, no `unwrap()`/`expect()` outside tests.
- Before every commit: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Conventional commit messages.
- Crate naming: package `czui-<name>` in `crates/<name>/`.
- Deferred out of this plan: symlink/dir content probing (Plan 3), all mutations except `chezmoi apply` (needed for tests), journal/daemon/UI.

## File Structure

```
Cargo.toml                      # workspace
rust-toolchain.toml
crates/core/
  Cargo.toml                    # package czui-core
  src/lib.rs
  src/cmd.rs                    # CommandRunner trait + SystemRunner
  src/cmd/fake.rs               # FakeRunner (always compiled; used by dependent crates' tests)
  src/chezmoi.rs                # ChezmoiClient + parsers + error classification
  src/git.rs                    # GitClient (read ops + scratch-repo test helper)
  src/drift.rs                  # ContentHash, DriftClass, StateProbe, classify()
  src/scanner.rs                # DriftScanner composing the above
  src/bin/drift-scan.rs         # debug binary
  tests/support/mod.rs          # scratch chezmoi home for integration tests
  tests/scanner_integration.rs
```

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `crates/core/Cargo.toml`, `crates/core/src/lib.rs`

**Interfaces:**
- Produces: compiling empty workspace; module skeleton `czui_core::{cmd, chezmoi, git, drift, scanner}` filled in by Tasks 2–6.

- [x] **Step 1: Create workspace files**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/core"]

[workspace.package]
edition = "2024"
license = "MIT"

[workspace.dependencies]
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
wait-timeout = "0.2"
tempfile = "3"
```

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "stable"
```

`crates/core/Cargo.toml`:
```toml
[package]
name = "czui-core"
version = "0.0.1"
edition.workspace = true
license.workspace = true

[lib]
name = "czui_core"

[dependencies]
thiserror.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
wait-timeout.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

`crates/core/src/lib.rs`:
```rust
pub mod chezmoi;
pub mod cmd;
pub mod drift;
pub mod git;
pub mod scanner;
```

Create empty placeholder files so it compiles: `src/cmd.rs`, `src/chezmoi.rs`, `src/git.rs`, `src/drift.rs`, `src/scanner.rs` (each may start with just `//! see plan task N`).

- [x] **Step 2: Verify it builds**

Run: `cargo check --workspace`
Expected: `Finished` with no errors.

- [x] **Step 3: Commit**

```bash
git add Cargo.toml rust-toolchain.toml crates
git commit -m "chore: scaffold cargo workspace with czui-core"
```

---

### Task 2: CommandRunner seam

**Files:**
- Modify: `crates/core/src/cmd.rs`
- Create: `crates/core/src/cmd/fake.rs`

**Interfaces:**
- Produces (used by every later task):
  - `CommandRequest { program: String, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<PathBuf>, stdin: Option<Vec<u8>>, timeout: Duration }` with builder-ish `CommandRequest::new(program) -> Self` and chainable `arg/args/env/cwd/stdin/timeout` methods.
  - `CommandOutput { exit_code: i32, stdout: Vec<u8>, stderr: Vec<u8> }` + `stdout_utf8() -> String`, `stderr_utf8() -> String`, `success() -> bool`.
  - `CommandError::{Spawn, Timeout, Io}` (thiserror).
  - `trait CommandRunner: Send + Sync { fn run(&self, req: CommandRequest) -> Result<CommandOutput, CommandError>; }`
  - `SystemRunner` (real), `fake::FakeRunner` (queued responses + recorded calls).

- [x] **Step 1: Write the failing tests** (in `cmd.rs` `#[cfg(test)]` module)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn system_runner_captures_stdout_and_exit_code() {
        let out = SystemRunner
            .run(CommandRequest::new("/bin/echo").arg("hi"))
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_utf8().trim(), "hi");
    }

    #[test]
    fn system_runner_kills_on_timeout() {
        let err = SystemRunner
            .run(CommandRequest::new("/bin/sleep").arg("5").timeout(Duration::from_millis(150)))
            .unwrap_err();
        assert!(matches!(err, CommandError::Timeout { .. }));
    }

    #[test]
    fn system_runner_pipes_stdin() {
        let out = SystemRunner
            .run(CommandRequest::new("/bin/cat").stdin(b"data".to_vec()))
            .unwrap();
        assert_eq!(out.stdout, b"data");
    }

    #[test]
    fn fake_runner_replays_and_records() {
        let fake = fake::FakeRunner::new();
        fake.push_ok(0, "out", "");
        let out = fake.run(CommandRequest::new("chezmoi").arg("managed")).unwrap();
        assert_eq!(out.stdout_utf8(), "out");
        let calls = fake.calls();
        assert_eq!(calls[0].args, vec!["managed"]);
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core cmd`
Expected: compile errors — types not defined.

- [x] **Step 3: Implement**

`crates/core/src/cmd.rs`:
```rust
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

pub mod fake;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct CommandRequest {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl CommandRequest {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            stdin: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, it: I) -> Self {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.cwd = Some(p.into());
        self
    }
    pub fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
    pub fn stdout_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} timed out after {timeout:?}")]
    Timeout { program: String, timeout: Duration },
    #[error("i/o error running {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, req: CommandRequest) -> Result<CommandOutput, CommandError>;
}

pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, req: CommandRequest) -> Result<CommandOutput, CommandError> {
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args)
            .stdin(if req.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().map_err(|source| CommandError::Spawn {
            program: req.program.clone(),
            source,
        })?;
        let io_err = |source| CommandError::Io { program: req.program.clone(), source };
        if let Some(bytes) = &req.stdin {
            let mut pipe = child.stdin.take().ok_or_else(|| CommandError::Io {
                program: req.program.clone(),
                source: std::io::Error::other("stdin pipe missing"),
            })?;
            pipe.write_all(bytes).map_err(io_err)?;
            // pipe drops here, closing stdin
        }
        // Read pipes on threads to avoid deadlock on full pipe buffers.
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");
        let out_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stdout_pipe.read_to_end(&mut buf).map(|_| buf)
        });
        let err_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stderr_pipe.read_to_end(&mut buf).map(|_| buf)
        });
        let status = match child.wait_timeout(req.timeout).map_err(io_err)? {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandError::Timeout {
                    program: req.program.clone(),
                    timeout: req.timeout,
                });
            }
        };
        let stdout = out_handle.join().expect("stdout reader panicked").map_err(io_err)?;
        let stderr = err_handle.join().expect("stderr reader panicked").map_err(io_err)?;
        Ok(CommandOutput {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }
}
```

Note: the two `expect()`s on `take()` and thread-join are invariants of this function (pipes were configured three lines above; reader threads don't panic) — acceptable per the constraints as they cannot fire from external input. If clippy objects, convert to `CommandError::Io` like the stdin case.

`crates/core/src/cmd/fake.rs`:
```rust
use std::collections::VecDeque;
use std::sync::Mutex;

use super::{CommandError, CommandOutput, CommandRequest, CommandRunner};

/// Test double: replays queued responses and records every request.
/// Lives in the main crate (not cfg(test)) so dependent crates' tests can use it.
#[derive(Default)]
pub struct FakeRunner {
    queue: Mutex<VecDeque<Result<CommandOutput, CommandError>>>,
    calls: Mutex<Vec<CommandRequest>>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push_ok(&self, exit_code: i32, stdout: &str, stderr: &str) {
        self.queue.lock().unwrap().push_back(Ok(CommandOutput {
            exit_code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }));
    }
    pub fn push_err(&self, err: CommandError) {
        self.queue.lock().unwrap().push_back(Err(err));
    }
    pub fn calls(&self) -> Vec<CommandRequest> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, req: CommandRequest) -> Result<CommandOutput, CommandError> {
        self.calls.lock().unwrap().push(req.clone());
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeRunner: unexpected command: {} {:?}", req.program, req.args))
    }
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core cmd`
Expected: 4 passed.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/cmd.rs crates/core/src/cmd
git commit -m "feat(core): CommandRunner seam with SystemRunner and FakeRunner"
```

---

### Task 3: ChezmoiClient

**Files:**
- Modify: `crates/core/src/chezmoi.rs`

**Interfaces:**
- Consumes: `cmd::{CommandRunner, CommandRequest, CommandError}`, `fake::FakeRunner` (tests).
- Produces:
  - `ChezmoiOptions { bin: String, base_args: Vec<String>, env: Vec<(String, String)>, timeout: Duration }` with `Default` (bin `"chezmoi"`, 30s).
  - `ChezmoiClient::new(runner: Arc<dyn CommandRunner>, opts: ChezmoiOptions) -> Self`
  - `source_dir() -> Result<PathBuf, ChezmoiError>`
  - `managed() -> Result<Vec<PathBuf>, ChezmoiError>` (absolute)
  - `status() -> Result<Vec<StatusEntry>, ChezmoiError>` (absolute paths)
  - `cat(target: &Path) -> Result<Vec<u8>, ChezmoiError>`
  - `source_path(target: &Path) -> Result<PathBuf, ChezmoiError>`
  - `target_paths(sources: &[PathBuf]) -> Result<Vec<PathBuf>, ChezmoiError>`
  - `state_dump() -> Result<StateDump, ChezmoiError>`
  - `execute_template(input: &[u8]) -> Result<Vec<u8>, ChezmoiError>`
  - `apply(target: Option<&Path>) -> Result<(), ChezmoiError>`
  - `StatusEntry { last_written_vs_actual: ChangeCode, actual_vs_target: ChangeCode, path: PathBuf }`, `ChangeCode::{None, Added, Deleted, Modified, Run}`
  - `StateDump { entry_state: HashMap<PathBuf, EntryState> }`, `EntryState { kind: String, contents_sha256: Option<String>, mode: Option<u32> }`
  - `ChezmoiError::{Eval(EvalFailure), Command(CommandError), Exit { code, stderr }, Parse { context, detail }}`
  - `EvalFailure { kind: EvalFailureKind, raw_stderr: String, hint: String }`, `EvalFailureKind::{OnePasswordMultipleAccounts, OnePasswordAuth, AgeDecrypt, GpgDecrypt, TemplateError, Unknown}`
  - `classify_eval_stderr(stderr: &str) -> Option<EvalFailure>` (pub — the scanner and daemon reuse it)

- [x] **Step 1: Write the failing tests** (`#[cfg(test)]` in `chezmoi.rs`)

The 1Password fixture below is verbatim from this machine (2026-07-19):

```rust
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
        assert!(call.env.contains(&("OP_ACCOUNT".to_string(), "acct".to_string())));
    }

    #[test]
    fn target_paths_maps_lines() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/Users/x/.zshrc\n/Users/x/.config/a\n", "");
        let out = client(fake)
            .target_paths(&["/s/dot_zshrc".into(), "/s/dot_config/a".into()])
            .unwrap();
        assert_eq!(out, vec![std::path::PathBuf::from("/Users/x/.zshrc"), "/Users/x/.config/a".into()]);
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core chezmoi`
Expected: compile errors — types not defined.

- [x] **Step 3: Implement**

`crates/core/src/chezmoi.rs`:
```rust
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
            base_args: Vec::new(),
            env: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }
}

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
    Parse { context: &'static str, detail: String },
}

/// Classify stderr from a failed chezmoi invocation into a structured
/// evaluation failure (spec §4 EvalFailed, §9 remediation hints).
pub fn classify_eval_stderr(stderr: &str) -> Option<EvalFailure> {
    let mk = |kind, hint: &str| {
        Some(EvalFailure { kind, raw_stderr: stderr.to_string(), hint: hint.to_string() })
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
        return mk(EvalFailureKind::AgeDecrypt, "age decryption failed — check identity file.");
    }
    if stderr.contains("gpg:") {
        return mk(EvalFailureKind::GpgDecrypt, "gpg decryption failed — check keyring.");
    }
    if stderr.contains("template:") {
        return mk(EvalFailureKind::TemplateError, "Template failed to render — see raw error.");
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
        let mut req = self.request(args);
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
        Err(ChezmoiError::Exit { code: out.exit_code, stderr })
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
                return Err(ChezmoiError::Parse { context: "status", detail: line.to_string() });
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
            self.run_utf8(&["source-path", &target.to_string_lossy()])?.trim(),
        ))
    }

    pub fn target_paths(&self, sources: &[PathBuf]) -> Result<Vec<PathBuf>, ChezmoiError> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = vec!["target-path".into()];
        args.extend(sources.iter().map(|p| p.to_string_lossy().into_owned()));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Ok(self.run_utf8(&arg_refs)?.lines().map(PathBuf::from).collect())
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
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core chezmoi`
Expected: 5 passed.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/chezmoi.rs
git commit -m "feat(core): ChezmoiClient with status/state parsers and eval-failure classification"
```

---

### Task 4: GitClient (read operations)

**Files:**
- Modify: `crates/core/src/git.rs`

**Interfaces:**
- Consumes: `cmd::*`.
- Produces:
  - `GitClient::new(runner: Arc<dyn CommandRunner>, repo: PathBuf) -> Self` (bin `"git"`, 30s timeout; fetch gets 120s)
  - `fetch(remote: &str) -> Result<(), GitError>`
  - `head_branch() -> Result<String, GitError>` (e.g. `"main"`)
  - `divergence(upstream: &str) -> Result<Divergence, GitError>` — `Divergence { ahead: u32, behind: u32 }` (HEAD vs upstream)
  - `changed_files(from: &str, to: &str) -> Result<Vec<PathBuf>, GitError>` (repo-relative, `git diff --name-only from..to`)
  - `blob_at(rev: &str, rel_path: &Path) -> Result<Option<Vec<u8>>, GitError>` (None if absent at rev)
  - `commits_touching(range: &str, rel_path: &Path) -> Result<u32, GitError>` (`git rev-list --count <range> -- <path>`)
  - `dirty_files() -> Result<Vec<PathBuf>, GitError>` (porcelain, repo-relative)
  - `GitError::{Command(CommandError), Exit { code, stderr }, Parse { context, detail }}`
- Tests build **real temp repos** with `SystemRunner` — hermetic, no fakes.

- [x] **Step 1: Write the failing tests** (`#[cfg(test)]` in `git.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CommandRequest, CommandRunner, SystemRunner};
    use std::path::Path;
    use std::sync::Arc;

    fn sh(cwd: &Path, program: &str, args: &[&str]) {
        let out = SystemRunner
            .run(CommandRequest::new(program).args(args.iter().copied()).cwd(cwd))
            .unwrap();
        assert!(out.success(), "{program} {args:?} failed: {}", out.stderr_utf8());
    }

    fn git(cwd: &Path, args: &[&str]) {
        // -c: identity without touching global config
        let mut full = vec!["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
        full.extend_from_slice(args);
        sh(cwd, "git", &full);
    }

    /// repo with an `origin` bare remote and one pushed commit (file `f.txt` = "one\n")
    fn scratch() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("origin.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(dir.path(), "git", &["init", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(&work, &["init", "-b", "main"]);
        std::fs::write(work.join("f.txt"), "one\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "c1"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "-u", "origin", "main"]);
        (dir, work)
    }

    fn client(repo: &Path) -> GitClient {
        GitClient::new(Arc::new(SystemRunner), repo.to_path_buf())
    }

    #[test]
    fn head_branch_and_clean_divergence() {
        let (_g, work) = scratch();
        let c = client(&work);
        assert_eq!(c.head_branch().unwrap(), "main");
        let d = c.divergence("origin/main").unwrap();
        assert_eq!((d.ahead, d.behind), (0, 0));
    }

    #[test]
    fn detects_remote_ahead_after_fetch() {
        let (guard, work) = scratch();
        // second clone pushes a change
        let other = guard.path().join("other");
        sh(guard.path(), "git", &["clone", work.join("../origin.git").to_str().unwrap(), other.to_str().unwrap()]);
        std::fs::write(other.join("f.txt"), "two\n").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-m", "c2"]);
        git(&other, &["push"]);

        let c = client(&work);
        c.fetch("origin").unwrap();
        let d = c.divergence("origin/main").unwrap();
        assert_eq!((d.ahead, d.behind), (0, 1));
        assert_eq!(c.changed_files("HEAD", "origin/main").unwrap(), vec![std::path::PathBuf::from("f.txt")]);
        assert_eq!(c.commits_touching("HEAD..origin/main", Path::new("f.txt")).unwrap(), 1);
        assert_eq!(c.blob_at("origin/main", Path::new("f.txt")).unwrap().unwrap(), b"two\n");
        assert_eq!(c.blob_at("origin/main", Path::new("missing.txt")).unwrap(), None);
    }

    #[test]
    fn dirty_files_lists_worktree_changes() {
        let (_g, work) = scratch();
        std::fs::write(work.join("f.txt"), "dirty\n").unwrap();
        assert_eq!(client(&work).dirty_files().unwrap(), vec![std::path::PathBuf::from("f.txt")]);
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core git`
Expected: compile errors — types not defined.

- [x] **Step 3: Implement**

`crates/core/src/git.rs`:
```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::cmd::{CommandError, CommandRequest, CommandRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("git exited with code {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("failed to parse git output ({context}): {detail}")]
    Parse { context: &'static str, detail: String },
}

pub struct GitClient {
    runner: Arc<dyn CommandRunner>,
    repo: PathBuf,
}

impl GitClient {
    pub fn new(runner: Arc<dyn CommandRunner>, repo: PathBuf) -> Self {
        Self { runner, repo }
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Result<Vec<u8>, GitError> {
        let out = self.runner.run(
            CommandRequest::new("git")
                .args(args.iter().copied())
                .cwd(&self.repo)
                .timeout(timeout),
        )?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(GitError::Exit { code: out.exit_code, stderr: out.stderr_utf8() })
        }
    }

    fn run_utf8(&self, args: &[&str]) -> Result<String, GitError> {
        Ok(String::from_utf8_lossy(&self.run(args, Duration::from_secs(30))?).into_owned())
    }

    pub fn fetch(&self, remote: &str) -> Result<(), GitError> {
        self.run(&["fetch", "--quiet", remote], Duration::from_secs(120))?;
        Ok(())
    }

    pub fn head_branch(&self) -> Result<String, GitError> {
        Ok(self.run_utf8(&["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string())
    }

    pub fn divergence(&self, upstream: &str) -> Result<Divergence, GitError> {
        let range = format!("{upstream}...HEAD");
        let text = self.run_utf8(&["rev-list", "--left-right", "--count", &range])?;
        // output: "<behind>\t<ahead>" (left = upstream-only, right = HEAD-only)
        let parts: Vec<&str> = text.split_whitespace().collect();
        let [behind, ahead] = parts.as_slice() else {
            return Err(GitError::Parse { context: "divergence", detail: text });
        };
        let parse = |s: &str| {
            s.parse::<u32>()
                .map_err(|e| GitError::Parse { context: "divergence", detail: e.to_string() })
        };
        Ok(Divergence { behind: parse(behind)?, ahead: parse(ahead)? })
    }

    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<PathBuf>, GitError> {
        let range = format!("{from}..{to}");
        Ok(self
            .run_utf8(&["diff", "--name-only", &range])?
            .lines()
            .map(PathBuf::from)
            .collect())
    }

    pub fn blob_at(&self, rev: &str, rel_path: &Path) -> Result<Option<Vec<u8>>, GitError> {
        let spec = format!("{rev}:{}", rel_path.to_string_lossy());
        match self.run(&["cat-file", "blob", &spec], Duration::from_secs(30)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(GitError::Exit { .. }) => Ok(None), // path absent at rev
            Err(e) => Err(e),
        }
    }

    pub fn commits_touching(&self, range: &str, rel_path: &Path) -> Result<u32, GitError> {
        let text = self.run_utf8(&[
            "rev-list",
            "--count",
            range,
            "--",
            &rel_path.to_string_lossy(),
        ])?;
        text.trim()
            .parse()
            .map_err(|e: std::num::ParseIntError| GitError::Parse {
                context: "commits_touching",
                detail: e.to_string(),
            })
    }

    pub fn dirty_files(&self) -> Result<Vec<PathBuf>, GitError> {
        Ok(self
            .run_utf8(&["status", "--porcelain"])?
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| PathBuf::from(l[3..].trim_start()))
            .collect())
    }
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core git`
Expected: 3 passed.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/git.rs
git commit -m "feat(core): GitClient read operations with hermetic temp-repo tests"
```

---

### Task 5: Drift domain model & classifier

**Files:**
- Modify: `crates/core/src/drift.rs`

**Interfaces:**
- Consumes: `chezmoi::EvalFailure`.
- Produces (spec §4):
  - `ContentHash([u8; 32])` with `ContentHash::of(bytes: &[u8]) -> Self` (sha256), `from_hex(&str) -> Option<Self>`, `to_hex() -> String`, `PartialEq/Eq/Clone/Copy/Debug`.
  - `GitSignals { local_ahead: bool, remote_ahead: bool }`
  - `StateProbe { destination: Option<ContentHash>, rendered: Result<Option<ContentHash>, EvalFailure>, last_written: Option<ContentHash>, git: GitSignals }`
  - `DriftClass::{InSync, DestinationDrift, SourceAhead, RemoteAhead, LocalSourceDiverged, Conflict, EvalFailed}`
  - `classify(probe: &StateProbe) -> DriftClass`

Classification rules (verbatim from spec §4, resolved to logic):
1. `rendered` is `Err` → `EvalFailed`.
2. `dest_drift` = destination ≠ last_written; `source_moved` = rendered ≠ last_written; `remote` = git.remote_ahead.
3. git.local_ahead && git.remote_ahead → `LocalSourceDiverged`, upgraded to `Conflict` if `dest_drift`.
4. Else count {dest_drift, source_moved, remote}: 0 → `InSync`; exactly one → the matching single class; ≥2 → `Conflict`.

- [x] **Step 1: Write the failing table-driven test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chezmoi::{EvalFailure, EvalFailureKind};

    fn h(n: u8) -> Option<ContentHash> {
        Some(ContentHash::of(&[n]))
    }
    fn probe(
        dest: Option<ContentHash>,
        rendered: Option<ContentHash>,
        last: Option<ContentHash>,
        local_ahead: bool,
        remote_ahead: bool,
    ) -> StateProbe {
        StateProbe {
            destination: dest,
            rendered: Ok(rendered),
            last_written: last,
            git: GitSignals { local_ahead, remote_ahead },
        }
    }

    #[test]
    fn classification_table() {
        use DriftClass::*;
        let cases = [
            (probe(h(1), h(1), h(1), false, false), InSync),
            (probe(h(2), h(1), h(1), false, false), DestinationDrift),
            (probe(None, h(1), h(1), false, false), DestinationDrift), // deleted on disk
            (probe(h(1), h(2), h(1), false, false), SourceAhead),
            (probe(h(1), h(1), h(1), false, true), RemoteAhead),
            (probe(h(1), h(1), h(1), true, true), LocalSourceDiverged),
            (probe(h(2), h(1), h(1), true, true), Conflict), // diverged + dest drift
            (probe(h(2), h(3), h(1), false, false), Conflict), // dest + source moved
            (probe(h(2), h(1), h(1), false, true), Conflict), // dest + remote
            (probe(h(1), h(1), None, false, false), Conflict), // never applied but present: dest+source signals
        ];
        for (i, (p, expected)) in cases.iter().enumerate() {
            assert_eq!(classify(p), *expected, "case {i}");
        }
    }

    #[test]
    fn eval_failure_dominates() {
        let p = StateProbe {
            destination: h(2),
            rendered: Err(EvalFailure {
                kind: EvalFailureKind::TemplateError,
                raw_stderr: String::new(),
                hint: String::new(),
            }),
            last_written: h(1),
            git: GitSignals { local_ahead: true, remote_ahead: true },
        };
        assert_eq!(classify(&p), DriftClass::EvalFailed);
    }

    #[test]
    fn hash_roundtrip() {
        let h = ContentHash::of(b"abc");
        assert_eq!(ContentHash::from_hex(&h.to_hex()), Some(h));
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core drift`
Expected: compile errors.

- [x] **Step 3: Implement**

`crates/core/src/drift.rs`:
```rust
use sha2::{Digest, Sha256};

use crate::chezmoi::EvalFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(digest.into())
    }
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Self(out))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitSignals {
    pub local_ahead: bool,
    pub remote_ahead: bool,
}

#[derive(Debug, Clone)]
pub struct StateProbe {
    /// Hash of the file at its target path; None = missing.
    pub destination: Option<ContentHash>,
    /// Hash of `chezmoi cat` output; Err = template/secret failure; Ok(None) = entry has no content (e.g. would be removed).
    pub rendered: Result<Option<ContentHash>, EvalFailure>,
    /// From `chezmoi state dump` entryState contentsSHA256; None = chezmoi never wrote this entry.
    pub last_written: Option<ContentHash>,
    pub git: GitSignals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftClass {
    InSync,
    DestinationDrift,
    SourceAhead,
    RemoteAhead,
    LocalSourceDiverged,
    Conflict,
    EvalFailed,
}

pub fn classify(probe: &StateProbe) -> DriftClass {
    let rendered = match &probe.rendered {
        Err(_) => return DriftClass::EvalFailed,
        Ok(r) => *r,
    };
    let dest_drift = probe.destination != probe.last_written;
    let source_moved = rendered != probe.last_written;
    if probe.git.local_ahead && probe.git.remote_ahead {
        return if dest_drift { DriftClass::Conflict } else { DriftClass::LocalSourceDiverged };
    }
    let signals =
        u8::from(dest_drift) + u8::from(source_moved) + u8::from(probe.git.remote_ahead);
    match signals {
        0 => DriftClass::InSync,
        1 if dest_drift => DriftClass::DestinationDrift,
        1 if source_moved => DriftClass::SourceAhead,
        1 => DriftClass::RemoteAhead,
        _ => DriftClass::Conflict,
    }
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core drift`
Expected: 3 passed.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/drift.rs
git commit -m "feat(core): four-state drift model and pure classifier"
```

---

### Task 6: DriftScanner

**Files:**
- Modify: `crates/core/src/scanner.rs`
- Create: `crates/core/tests/support/mod.rs`, `crates/core/tests/scanner_integration.rs`

**Interfaces:**
- Consumes: `ChezmoiClient` (Task 3), `GitClient` (Task 4), `classify`/`StateProbe`/`DriftClass`/`ContentHash` (Task 5).
- Produces:
  - `DriftScanner::new(chezmoi: ChezmoiClient, git: GitClient, remote_ref: String) -> Self` (`remote_ref` like `"origin/main"`)
  - `scan() -> Result<ScanReport, ScanError>`
  - `ScanReport { drifted: Vec<FileDrift>, in_sync_count: usize, degraded: Option<EvalFailure> }`
  - `FileDrift { target: PathBuf, source_rel: Option<PathBuf>, class: DriftClass, probe: StateProbe }`
  - `ScanError::{Chezmoi(ChezmoiError), Git(GitError), Io(std::io::Error)}`

**Algorithm (documented in scanner.rs doc comment):**
1. `source_dir`, `managed`, `state_dump`.
2. Candidate discovery, cheap first:
   a. *Destination side without rendering*: for every dump entry with `contents_sha256` (files only in this plan), hash the file at the target path; mismatch or missing → candidate. This reproduces status column 1 without touching templates.
   b. *Source side via git*: `changed_files("HEAD", remote_ref)` + `dirty_files()` → map to targets via `target_paths()` → candidates.
   c. *Rendered side*: try `status()`; union its paths in. If it fails with `ChezmoiError::Eval`, set `degraded = Some(failure)` and continue with a+b only (spec §10: degrade, never silently drop).
3. Per candidate (managed files only): build `StateProbe` — destination hash from fs; rendered from `cat` (per-file `Eval` error → `Err(EvalFailure)` in the probe); last_written from dump (`ContentHash::from_hex`); `GitSignals` from `commits_touching("{remote}..HEAD", src_rel)` / `commits_touching("HEAD..{remote}", src_rel)`.
4. `classify`, collect non-`InSync` into `drifted`; `in_sync_count = managed_files - drifted.len()`.

- [x] **Step 1: Write the scratch-home support helper**

`crates/core/tests/support/mod.rs`:
```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use czui_core::git::GitClient;
use czui_core::scanner::DriftScanner;

pub struct Scratch {
    pub root: tempfile::TempDir,
    pub home: PathBuf,
    pub source: PathBuf,
    pub bare: PathBuf,
}

pub fn sh(cwd: &Path, program: &str, args: &[&str]) {
    let out = SystemRunner
        .run(CommandRequest::new(program).args(args.iter().copied()).cwd(cwd))
        .unwrap();
    assert!(out.success(), "{program} {args:?}: {}", out.stderr_utf8());
}

pub fn git(cwd: &Path, args: &[&str]) {
    let mut full = vec!["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
    full.extend_from_slice(args);
    sh(cwd, "git", &full);
}

impl Scratch {
    /// chezmoi home with one managed file `~/.testrc` (source `dot_testrc` = "a=1\n"),
    /// applied, committed, and pushed to a local bare `origin`.
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let source = root.path().join("source");
        let bare = root.path().join("origin.git");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        sh(root.path(), "git", &["init", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("dot_testrc"), "a=1\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "init"]);
        git(&source, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&source, &["push", "-u", "origin", "main"]);
        let s = Self { root, home, source, bare };
        s.chezmoi().apply(None).expect("initial apply (is chezmoi installed?)");
        s
    }

    pub fn chezmoi(&self) -> ChezmoiClient {
        let opts = ChezmoiOptions {
            base_args: vec![
                "--source".into(),
                self.source.to_string_lossy().into_owned(),
                "--destination".into(),
                self.home.to_string_lossy().into_owned(),
                "--config".into(),
                self.config_path().to_string_lossy().into_owned(),
                "--no-tty".into(),
                "--no-pager".into(),
            ],
            ..ChezmoiOptions::default()
        };
        ChezmoiClient::new(Arc::new(SystemRunner), opts)
    }

    fn config_path(&self) -> PathBuf {
        let p = self.root.path().join("chezmoi.toml");
        if !p.exists() {
            std::fs::write(&p, "").unwrap();
        }
        p
    }

    pub fn scanner(&self) -> DriftScanner {
        DriftScanner::new(
            self.chezmoi(),
            GitClient::new(Arc::new(SystemRunner), self.source.clone()),
            "origin/main".to_string(),
        )
    }
}
```

- [x] **Step 2: Write the failing integration tests**

`crates/core/tests/scanner_integration.rs`:
```rust
mod support;

use czui_core::drift::DriftClass;
use support::{git, Scratch};

#[test]
fn clean_state_is_in_sync() {
    let s = Scratch::new();
    let report = s.scanner().scan().unwrap();
    assert!(report.drifted.is_empty(), "{:?}", report.drifted);
    assert_eq!(report.in_sync_count, 1);
    assert!(report.degraded.is_none());
}

#[test]
fn tool_rewrite_is_destination_drift() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=2\n").unwrap();
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    let d = &report.drifted[0];
    assert!(d.target.ends_with(".testrc"));
    assert_eq!(d.class, DriftClass::DestinationDrift);
}

#[test]
fn source_edit_is_source_ahead() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_testrc"), "a=3\n").unwrap();
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::SourceAhead);
}

#[test]
fn remote_push_is_remote_ahead() {
    let s = Scratch::new();
    let other = s.root.path().join("other");
    support::sh(s.root.path(), "git", &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("dot_testrc"), "a=4\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    // scanner does not fetch; the caller owns fetch cadence (spec §3.1)
    support::git(&s.source, &["fetch", "origin"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::RemoteAhead);
}

#[test]
fn disk_and_remote_change_is_conflict() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=local\n").unwrap();
    let other = s.root.path().join("other");
    support::sh(s.root.path(), "git", &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("dot_testrc"), "a=remote\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    support::git(&s.source, &["fetch", "origin"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::Conflict);
}
```

- [x] **Step 3: Run to verify failure**

Run: `cargo test -p czui-core --test scanner_integration`
Expected: compile errors (`DriftScanner` undefined).

- [x] **Step 4: Implement**

`crates/core/src/scanner.rs`:
```rust
//! Point-in-time drift scan composing ChezmoiClient + GitClient + classify().
//!
//! Candidate discovery is layered so template failures cannot hide drift:
//!   a) destination-vs-last-written via state dump hashes (no rendering),
//!   b) source-side via git (remote diff + dirty worktree) mapped through
//!      `chezmoi target-path`,
//!   c) `chezmoi status` when it works; if it fails with an EvalFailure the
//!      scan continues from (a)+(b) and reports `degraded`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::chezmoi::{ChezmoiClient, ChezmoiError, EvalFailure};
use crate::drift::{classify, ContentHash, DriftClass, GitSignals, StateProbe};
use crate::git::{GitClient, GitError};

pub struct DriftScanner {
    chezmoi: ChezmoiClient,
    git: GitClient,
    remote_ref: String,
}

#[derive(Debug)]
pub struct FileDrift {
    pub target: PathBuf,
    pub source_rel: Option<PathBuf>,
    pub class: DriftClass,
    pub probe: StateProbe,
}

#[derive(Debug)]
pub struct ScanReport {
    pub drifted: Vec<FileDrift>,
    pub in_sync_count: usize,
    /// Set when `chezmoi status` could not run (e.g. secret manager locked);
    /// the scan still covers destination- and git-side drift.
    pub degraded: Option<EvalFailure>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("i/o error during scan: {0}")]
    Io(#[from] std::io::Error),
}

fn hash_file(path: &Path) -> Result<Option<ContentHash>, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(ContentHash::of(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl DriftScanner {
    pub fn new(chezmoi: ChezmoiClient, git: GitClient, remote_ref: String) -> Self {
        Self { chezmoi, git, remote_ref }
    }

    pub fn scan(&self) -> Result<ScanReport, ScanError> {
        let source_dir = self.chezmoi.source_dir()?;
        let dump = self.chezmoi.state_dump()?;
        let managed: BTreeSet<PathBuf> = self.chezmoi.managed()?.into_iter().collect();

        let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();
        let mut managed_file_count = 0usize;

        // (a) destination side, render-free
        for (target, entry) in &dump.entry_state {
            if entry.kind != "file" || !managed.contains(target) {
                continue;
            }
            managed_file_count += 1;
            let expected = entry.contents_sha256.as_deref().and_then(ContentHash::from_hex);
            if hash_file(target)? != expected {
                candidates.insert(target.clone());
            }
        }

        // (b) source side via git
        let mut source_changed: BTreeSet<PathBuf> = BTreeSet::new();
        source_changed.extend(self.git.changed_files("HEAD", &self.remote_ref)?);
        source_changed.extend(self.git.dirty_files()?);
        // Map one source at a time: non-entry files (.chezmoiignore, README, …)
        // make `chezmoi target-path` fail, and must not kill the scan.
        for rel in &source_changed {
            let abs = source_dir.join(rel);
            let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
                continue;
            };
            for target in targets {
                if managed.contains(&target) {
                    candidates.insert(target);
                }
            }
        }

        // (c) rendered side via status, degrading on eval failure
        let mut degraded = None;
        match self.chezmoi.status() {
            Ok(entries) => {
                for e in entries {
                    if managed.contains(&e.path) {
                        candidates.insert(e.path);
                    }
                }
            }
            Err(ChezmoiError::Eval(f)) => degraded = Some(f),
            Err(other) => return Err(other.into()),
        }

        let mut drifted = Vec::new();
        for target in candidates {
            let Some(entry) = dump.entry_state.get(&target) else {
                // Managed but never written by chezmoi (fresh from remote, or
                // ignored entry type) — probe with last_written = None.
                if let Some(d) = self.probe_file(&source_dir, &target, None)? {
                    drifted.push(d);
                }
                continue;
            };
            if entry.kind != "file" {
                continue; // symlink/dir probing: Plan 3
            }
            let last = entry.contents_sha256.as_deref().and_then(ContentHash::from_hex);
            if let Some(d) = self.probe_file(&source_dir, &target, last)? {
                drifted.push(d);
            }
        }

        let in_sync_count = managed_file_count.saturating_sub(drifted.len());
        Ok(ScanReport { drifted, in_sync_count, degraded })
    }

    fn probe_file(
        &self,
        source_dir: &Path,
        target: &Path,
        last_written: Option<ContentHash>,
    ) -> Result<Option<FileDrift>, ScanError> {
        let destination = hash_file(target)?;
        let rendered = match self.chezmoi.cat(target) {
            Ok(bytes) => Ok(Some(ContentHash::of(&bytes))),
            Err(ChezmoiError::Eval(f)) => Err(f),
            Err(other) => return Err(other.into()),
        };
        let source_rel = match self.chezmoi.source_path(target) {
            Ok(abs) => abs.strip_prefix(source_dir).ok().map(Path::to_path_buf),
            Err(_) => None,
        };
        let git = match &source_rel {
            Some(rel) => GitSignals {
                local_ahead: self
                    .git
                    .commits_touching(&format!("{}..HEAD", self.remote_ref), rel)?
                    > 0,
                remote_ahead: self
                    .git
                    .commits_touching(&format!("HEAD..{}", self.remote_ref), rel)?
                    > 0,
            },
            None => GitSignals::default(),
        };
        let probe = StateProbe { destination, rendered, last_written, git };
        let class = classify(&probe);
        if class == DriftClass::InSync {
            return Ok(None);
        }
        Ok(Some(FileDrift { target: target.to_path_buf(), source_rel, class, probe }))
    }
}
```

- [x] **Step 5: Run tests**

Run: `cargo test -p czui-core --test scanner_integration`
Expected: 5 passed. (Requires `chezmoi` and `git` on PATH; the `Scratch::new` expect message says so if missing.)

- [x] **Step 6: Run the full suite**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [x] **Step 7: Commit**

```bash
git add crates/core/src/scanner.rs crates/core/tests
git commit -m "feat(core): DriftScanner with degradation on eval failure and scratch-home integration tests"
```

---

### Task 7: `drift-scan` debug binary

**Files:**
- Create: `crates/core/src/bin/drift-scan.rs`

**Interfaces:**
- Consumes: `DriftScanner`, `ChezmoiClient`, `GitClient`, `SystemRunner`.
- Produces: `cargo run -p czui-core --bin drift-scan [--fetch] [--json]` against the real machine. This is Plan 1's working-software deliverable and the manual smoke tool for later plans.

- [x] **Step 1: Implement** (debug tool: `unwrap`-style exits via error printing are fine here — it's a bin, not a lib)

```rust
use std::process::ExitCode;
use std::sync::Arc;

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_core::scanner::DriftScanner;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let fetch = args.iter().any(|a| a == "--fetch");

    let runner = Arc::new(SystemRunner);
    let chezmoi = ChezmoiClient::new(runner.clone(), ChezmoiOptions::default());
    let source_dir = match chezmoi.source_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot locate chezmoi source dir: {e}");
            return ExitCode::FAILURE;
        }
    };
    let git = GitClient::new(runner, source_dir);
    let branch = git.head_branch().unwrap_or_else(|_| "main".into());
    let remote_ref = format!("origin/{branch}");
    if fetch {
        if let Err(e) = git.fetch("origin") {
            eprintln!("warning: fetch failed, remote info may be stale: {e}");
        }
    }
    let scanner = DriftScanner::new(chezmoi, git, remote_ref);
    match scanner.scan() {
        Ok(report) => {
            if json {
                // hand-rolled to avoid serde derives on domain types for now
                println!("{{\"drifted\":{},\"in_sync\":{},\"degraded\":{}}}",
                    report.drifted.len(), report.in_sync_count, report.degraded.is_some());
            }
            if let Some(f) = &report.degraded {
                eprintln!("degraded scan: {} — {}", f.hint, f.raw_stderr.lines().next().unwrap_or(""));
            }
            for d in &report.drifted {
                println!("{:<22} {}", format!("{:?}", d.class), d.target.display());
            }
            println!("-- {} drifted, {} in sync", report.drifted.len(), report.in_sync_count);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("scan failed: {e}");
            ExitCode::FAILURE
        }
    }
}
```

- [x] **Step 2: Build and run on the real machine**

Run: `cargo run -p czui-core --bin drift-scan`
Expected on this machine today: a `degraded scan:` line mentioning OP_ACCOUNT (the 1Password multi-account issue), plus any genuinely drifted files, plus the summary line. No panic, exit 0.

Run: `OP_ACCOUNT=<account> cargo run -p czui-core --bin drift-scan` (once an account shorthand is chosen)
Expected: no degraded line; full classification including rendered-side drift. (`ChezmoiOptions.env` injection is exercised by the daemon/app later; the binary inherits the process env, which chezmoi passes to `op`.)

- [x] **Step 3: Full suite + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

```bash
git add crates/core/src/bin
git commit -m "feat(core): drift-scan debug binary for real-machine smoke runs"
```

---

## Self-Review Notes (completed during plan writing)

- **Spec coverage for this plan's slice:** §3.4 core crate ✓ (Tasks 1–7); §4 domain model ✓ (Task 5; symlink/dir probing explicitly deferred to Plan 3); §10 subprocess discipline ✓ (Task 2 timeouts, Task 3 classification, scanner degradation); §9 OP_ACCOUNT env injection point ✓ (ChezmoiOptions.env, asserted in Task 3 test). Merge engine (§6), journal (§8), daemon (§3.1), UI (§7) are later plans by design.
- **Type consistency:** `ContentHash::from_hex` used by scanner matches Task 5 signature; `ChezmoiOptions.base_args` used by Task 6 support helper matches Task 3; `FakeRunner.calls()` matches Task 2.
- **Known simplifications (accepted for Plan 1):** scanner treats `status()` non-eval errors as fatal; `in_sync_count` counts files only; `drift-scan --json` output is a stub until `serde` derives land with the journal in Plan 3; `blob_at` maps any nonzero `git cat-file` exit to `None` (missing-at-rev and bad-rev are conflated).
- **Fix applied during review:** source→target mapping is per-file with skip-on-error, because non-entry source files (`.chezmoiignore`, `README.md`) make `chezmoi target-path` exit nonzero and must not abort the scan.
