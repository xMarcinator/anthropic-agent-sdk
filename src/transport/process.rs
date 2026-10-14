//! Starting the CLI process and following it until it exits.
//!
//! The SDK builds the command (program, arguments, environment, working directory) and owns the
//! process once it runs: its pipes, its exit and killing it. A [`ProcessSpawner`] decides only how
//! the process starts, for a caller that must contain it (a Windows Job Object, a Unix process
//! group) or start it through a wrapper executable.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};

/// The command the SDK wants started: the CLI and its arguments, the whole environment of the
/// process (nothing else is inherited) and its working directory.
#[derive(Clone)]
pub struct SpawnRequest {
    /// The CLI executable
    pub program: PathBuf,
    /// The CLI's arguments
    pub args: Vec<OsString>,
    /// The process's whole environment
    pub env: Vec<(String, String)>,
    /// The working directory, when one is set
    pub cwd: Option<PathBuf>,
}

impl fmt::Debug for SpawnRequest {
    /// Names the program only: arguments and environment can carry secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnRequest")
            .field("program", &self.program)
            .field("args", &format!("[{} arguments]", self.args.len()))
            .field("env", &format!("[{} variables]", self.env.len()))
            .field("cwd", &self.cwd)
            .finish()
    }
}

impl SpawnRequest {
    /// The request as a command with exactly its environment and piped stdin, stdout and
    /// stderr, which a [`ProcessSpawner`] may adjust (prefix arguments, creation flags) before
    /// it spawns it.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }
}

/// Starts the CLI process for the SDK.
///
/// The child must have piped stdin, stdout and stderr; [`SpawnRequest::command`] sets them up.
/// The SDK then writes to and reads from the child, waits for its exit and kills it when asked.
pub trait ProcessSpawner: Send + Sync {
    /// Starts `request`.
    ///
    /// # Errors
    /// The spawn failure, reported as the client's connection error.
    fn spawn(&self, request: SpawnRequest) -> std::io::Result<Child>;
}

/// The default spawner: `request.command()`, started as it is.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectSpawner;

impl ProcessSpawner for DirectSpawner {
    fn spawn(&self, request: SpawnRequest) -> std::io::Result<Child> {
        request.command().spawn()
    }
}

/// A shared [`ProcessSpawner`].
pub type SharedSpawner = Arc<dyn ProcessSpawner>;

/// How the CLI process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    /// The exit code; `None` when a signal ended it or its status could not be read
    pub code: Option<i32>,
}

/// The running CLI process: wait for its exit or kill it, from any task. Cloning shares it.
#[derive(Clone)]
pub struct ProcessHandle {
    exit: watch::Receiver<Option<ProcessExit>>,
    kill: Arc<Notify>,
    id: Option<u32>,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle")
            .field("id", &self.id)
            .field("exit", &*self.exit.borrow())
            .finish()
    }
}

impl ProcessHandle {
    /// Follows `child` in a task of its own until it exits or is killed.
    pub(crate) fn follow(mut child: Child) -> Self {
        let id = child.id();
        let (sender, exit) = watch::channel(None);
        let kill = Arc::new(Notify::new());
        let killed = kill.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                () = killed.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let code = status.ok().and_then(|status| status.code());
            let _ = sender.send(Some(ProcessExit { code }));
        });
        Self { exit, kill, id }
    }

    /// The process id, while the process had one at start.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.id
    }

    /// How the process ended, once it has.
    #[must_use]
    pub fn exited(&self) -> Option<ProcessExit> {
        *self.exit.borrow()
    }

    /// Waits for the process to exit.
    pub async fn wait(&self) -> ProcessExit {
        let mut exit = self.exit.clone();
        loop {
            if let Some(ended) = *exit.borrow_and_update() {
                return ended;
            }
            if exit.changed().await.is_err() {
                // The follower is gone without a status (its runtime stopped).
                return ProcessExit { code: None };
            }
        }
    }

    /// Kills the process (only the process itself: a [`ProcessSpawner`] that contains its
    /// children ends them its own way). Returns at once; [`Self::wait`] reports the exit.
    pub fn kill(&self) {
        self.kill.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_requests_never_print_arguments_or_environment() {
        let request = SpawnRequest {
            program: PathBuf::from("claude"),
            args: vec!["--mcp-config".into(), "secret-url".into()],
            env: vec![("ANTHROPIC_API_KEY".into(), "sk-secret".into())],
            cwd: None,
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert!(printed.contains("claude"));
    }
}
