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
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
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
        let stdout = out_handle
            .join()
            .expect("stdout reader panicked")
            .map_err(io_err)?;
        let stderr = err_handle
            .join()
            .expect("stderr reader panicked")
            .map_err(io_err)?;
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
