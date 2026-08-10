use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// How long a finished child's output pipes may stay open (held by lingering
/// grandchildren) before the whole process group is killed. Learned the hard
/// way: a hung helper spawned by chezmoi inherited our pipes, survived the
/// child's death, and wedged the reader joins for minutes.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// SIGKILL the child's entire process group (the child was made a group
/// leader via `process_group(0)`), so grandchildren die with it.
fn kill_group(child_pid: u32) {
    unsafe {
        libc::kill(-(child_pid as i32), libc::SIGKILL);
    }
}

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
    #[error(
        "{program} timed out after {timeout:?}; stderr tail: {stderr_tail:?}; stdout tail: {stdout_tail:?}"
    )]
    Timeout {
        program: String,
        timeout: Duration,
        /// Last output the child produced before the group kill — usually
        /// names exactly what it was doing or waiting for.
        stderr_tail: String,
        stdout_tail: String,
    },
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
        // Every subprocess through one logged chokepoint: most debugging
        // questions are "what exactly did chezmoi/git/op say" (2026-08-10).
        let t0 = std::time::Instant::now();
        let described = format!("{} {}", req.program, req.args.join(" "));
        let result = self.run_inner(req);
        match &result {
            Ok(out) if out.success() => {
                crate::log_info!("cmd", "{described} → ok in {:?}", t0.elapsed());
            }
            Ok(out) => {
                let stderr = out.stderr_utf8();
                let brief: String = stderr.chars().take(600).collect();
                crate::log_error!(
                    "cmd",
                    "{described} → exit {} in {:?}: {brief}",
                    out.exit_code,
                    t0.elapsed()
                );
            }
            Err(e) => {
                crate::log_error!("cmd", "{described} → {e} in {:?}", t0.elapsed());
            }
        }
        result
    }
}

impl SystemRunner {
    fn run_inner(&self, req: CommandRequest) -> Result<CommandOutput, CommandError> {
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args)
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group: on timeout (or lingering pipe-holders) the WHOLE
        // subprocess tree is killed, not just the direct child.
        cmd.process_group(0);
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
        let pid = child.id();
        let io_err = |source| CommandError::Io {
            program: req.program.clone(),
            source,
        };
        if let Some(bytes) = &req.stdin {
            let mut pipe = child.stdin.take().ok_or_else(|| CommandError::Io {
                program: req.program.clone(),
                source: std::io::Error::other("stdin pipe missing"),
            })?;
            pipe.write_all(bytes).map_err(io_err)?;
            // pipe drops here, closing stdin
        }
        // Read pipes on threads (avoids deadlock on full pipe buffers) and
        // collect through a channel so waiting is BOUNDED — a grandchild
        // holding the pipe open must never wedge us.
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");
        let (tx_out, rx_out) = channel();
        let (tx_err, rx_err) = channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let res = stdout_pipe.read_to_end(&mut buf).map(|_| buf);
            let _ = tx_out.send(res);
        });
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let res = stderr_pipe.read_to_end(&mut buf).map(|_| buf);
            let _ = tx_err.send(res);
        });

        // Poll for exit instead of wait-timeout's SIGCHLD self-pipe: signal
        // delivery races in a multithreaded daemon made waits miss exits and
        // burn the whole timeout while the child lay dead (the "chezmoi
        // timed out but its stdout has the answer" bug). WNOHANG polling has
        // no failure mode.
        let deadline = Instant::now() + req.timeout;
        let exit = loop {
            match child.try_wait().map_err(io_err)? {
                Some(status) => break Some(status),
                None if Instant::now() >= deadline => break None,
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        let status = match exit {
            Some(status) => status,
            None => {
                kill_group(pid);
                let _ = child.wait(); // reap; the group kill already landed
                // The group is dead → pipes closed → readers finish promptly.
                // Their partial buffers are the diagnostic evidence.
                let tail = |rx: &std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>| match rx
                    .recv_timeout(PIPE_GRACE)
                {
                    Ok(Ok(buf)) => {
                        let text = String::from_utf8_lossy(&buf);
                        let tail: String = text
                            .lines()
                            .rev()
                            .take(5)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join(" | ");
                        tail.chars().take(500).collect()
                    }
                    _ => String::from("<unavailable>"),
                };
                return Err(CommandError::Timeout {
                    program: req.program.clone(),
                    timeout: req.timeout,
                    stderr_tail: tail(&rx_err),
                    stdout_tail: tail(&rx_out),
                });
            }
        };

        // The child exited; give the pipes a short grace to close. If they
        // don't (a backgrounded grandchild inherited them), kill the group
        // and try once more — error rather than silently truncate.
        let collect = |rx: &std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
                       killed: &mut bool|
         -> Result<Vec<u8>, CommandError> {
            match rx.recv_timeout(PIPE_GRACE) {
                Ok(res) => res.map_err(io_err),
                Err(RecvTimeoutError::Timeout) => {
                    if !*killed {
                        kill_group(pid);
                        *killed = true;
                    }
                    match rx.recv_timeout(PIPE_GRACE) {
                        Ok(res) => res.map_err(io_err),
                        Err(_) => Err(CommandError::Io {
                            program: req.program.clone(),
                            source: std::io::Error::other(
                                "output pipes held open after exit (orphaned grandchild?)",
                            ),
                        }),
                    }
                }
                Err(RecvTimeoutError::Disconnected) => Err(CommandError::Io {
                    program: req.program.clone(),
                    source: std::io::Error::other("output reader died"),
                }),
            }
        };
        let mut killed = false;
        let stdout = collect(&rx_out, &mut killed)?;
        let stderr = collect(&rx_err, &mut killed)?;
        Ok(CommandOutput {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }
}

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
            .run(
                CommandRequest::new("/bin/sleep")
                    .arg("5")
                    .timeout(Duration::from_millis(150)),
            )
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
    fn timeout_kills_the_whole_process_group() {
        // A child that spawns a pipe-holding grandchild and hangs: the old
        // runner killed only the child and then wedged for minutes on the
        // reader joins (the 24h-stuck-daemon bug). Must return promptly.
        let start = std::time::Instant::now();
        let err = SystemRunner
            .run(
                CommandRequest::new("/bin/sh")
                    .arg("-c")
                    .arg("sleep 60 & sleep 60")
                    .timeout(Duration::from_millis(300)),
            )
            .unwrap_err();
        assert!(matches!(err, CommandError::Timeout { .. }), "{err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timeout path must not block on orphaned pipe holders: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn lingering_grandchild_after_exit_cannot_wedge_collection() {
        // Child exits 0 immediately but leaves a backgrounded grandchild
        // holding the stdout pipe. Collection must grace out, kill the
        // group, and still return the child's output.
        let start = std::time::Instant::now();
        let out = SystemRunner
            .run(
                CommandRequest::new("/bin/sh")
                    .arg("-c")
                    .arg("echo hi; sleep 60 &"),
            )
            .unwrap();
        assert_eq!(out.stdout_utf8().trim(), "hi");
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "success path must be bounded: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn fake_runner_replays_and_records() {
        let fake = fake::FakeRunner::new();
        fake.push_ok(0, "out", "");
        let out = fake
            .run(CommandRequest::new("chezmoi").arg("managed"))
            .unwrap();
        assert_eq!(out.stdout_utf8(), "out");
        let calls = fake.calls();
        assert_eq!(calls[0].args, vec!["managed"]);
    }
}

/// Union of the current PATH, the login shell's PATH, and existing brew
/// fallbacks — `None` when nothing new would be added. Pure so the merge
/// is testable; [`adopt_login_shell_path`] owns the process mutation.
pub fn merged_login_path(
    current: &std::ffi::OsStr,
    shell_path: &str,
    fallbacks: &[&str],
) -> Option<std::ffi::OsString> {
    let mut entries: Vec<PathBuf> = std::env::split_paths(current).collect();
    let mut changed = false;
    for p in std::env::split_paths(shell_path.trim()) {
        if !p.as_os_str().is_empty() && !entries.contains(&p) {
            entries.push(p);
            changed = true;
        }
    }
    for f in fallbacks {
        let p = PathBuf::from(f);
        if p.is_dir() && !entries.contains(&p) {
            entries.push(p);
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    std::env::join_paths(entries).ok()
}

/// GUI-launched processes inherit launchd's minimal PATH — no Homebrew,
/// no user tool dirs — so `op`/`chezmoi`/`git` exist in a terminal but
/// vanish inside the bundled .app (2026-08-08: the released build's
/// 1Password account picker errored on every machine). Harvest the login
/// shell's PATH once at startup, the same trick editors use. Best-effort
/// and bounded: a broken shell init must not wedge boot.
pub fn adopt_login_shell_path() {
    // Hermetic-test escape hatch: e2e suites that simulate missing tools
    // via a stripped PATH must not have it silently repaired.
    if std::env::var_os("TOMTE_NO_SHELL_PATH").is_some() {
        return;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let shell_path = SystemRunner
        .run(
            CommandRequest::new(&shell)
                .args(["-l", "-c", "printenv PATH"])
                .timeout(Duration::from_secs(5)),
        )
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout_utf8())
        .unwrap_or_default();
    let current = std::env::var_os("PATH").unwrap_or_default();
    if let Some(joined) = merged_login_path(
        &current,
        &shell_path,
        &["/opt/homebrew/bin", "/usr/local/bin"],
    ) {
        // Startup-only, before any threads spawn subprocesses.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

#[cfg(test)]
mod path_tests {
    use super::merged_login_path;
    use std::ffi::OsStr;

    #[test]
    fn merges_new_entries_and_skips_known_ones() {
        let merged =
            merged_login_path(OsStr::new("/usr/bin:/bin"), "/opt/x/bin:/usr/bin", &[]).unwrap();
        assert_eq!(merged.to_str().unwrap(), "/usr/bin:/bin:/opt/x/bin");
        // nothing new → None
        assert!(merged_login_path(OsStr::new("/usr/bin:/bin"), "/usr/bin", &[]).is_none());
    }
}
