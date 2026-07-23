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
        self.push_ok_bytes(exit_code, stdout.as_bytes(), stderr.as_bytes());
    }
    /// Raw-bytes variant for faking non-UTF-8 output (e.g. a binary
    /// `chezmoi cat`).
    pub fn push_ok_bytes(&self, exit_code: i32, stdout: &[u8], stderr: &[u8]) {
        self.queue.lock().unwrap().push_back(Ok(CommandOutput {
            exit_code,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
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
        self.queue.lock().unwrap().pop_front().unwrap_or_else(|| {
            panic!(
                "FakeRunner: unexpected command: {} {:?}",
                req.program, req.args
            )
        })
    }
}
