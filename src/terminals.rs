use anyhow::{Result, anyhow};
use dashmap::DashMap;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use uuid::Uuid;

pub struct Terminal {
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    id: String,
}

pub struct TerminalManager {
    terminals: DashMap<String, Terminal>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: DashMap::new(),
        }
    }

    pub fn create_terminal(&self) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let mut child = Command::new("bash")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn bash: {}", e))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("No stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("No stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("No stderr"))?;

        self.terminals.insert(
            id.clone(),
            Terminal {
                stdin,
                stdout,
                stderr,
                id: id.clone(),
            },
        );
        Ok(id)
    }

    pub async fn run_command(&self, terminal_id: &str, command: &str) -> Result<(String, String)> {
        if let Some(mut terminal_ref) = self.terminals.get_mut(terminal_id) {
            let terminal = terminal_ref.value_mut();
            let marker = format!("---MARKER-{}---", Uuid::new_v4());
            let full_command = format!("{}; echo {}; echo {} >&2\n", command, marker, marker);

            terminal.stdin.write_all(full_command.as_bytes()).await?;
            terminal.stdin.flush().await?;

            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            let mut tmp_stdout = [0u8; 1024];
            let mut tmp_stderr = [0u8; 1024];

            let mut stdout_done = false;
            let mut stderr_done = false;

            {
                let Terminal { stdout, stderr, .. } = terminal;

                while !stdout_done || !stderr_done {
                    tokio::select! {
                        res = stdout.read(&mut tmp_stdout), if !stdout_done => {
                            let n = res?;
                            if n == 0 { stdout_done = true; }
                            stdout_buf.extend_from_slice(&tmp_stdout[..n]);
                            if let Ok(s) = std::str::from_utf8(&stdout_buf) {
                                if s.contains(&marker) {
                                    stdout_done = true;
                                }
                            }
                        }
                        res = stderr.read(&mut tmp_stderr), if !stderr_done => {
                            let n = res?;
                            if n == 0 { stderr_done = true; }
                            stderr_buf.extend_from_slice(&tmp_stderr[..n]);
                            if let Ok(s) = std::str::from_utf8(&stderr_buf) {
                                if s.contains(&marker) {
                                    stderr_done = true;
                                }
                            }
                        }
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                            return Err(anyhow!("Command timed out"));
                        }
                    }
                }
            }

            let stdout = String::from_utf8_lossy(&stdout_buf)
                .replace(&marker, "")
                .trim()
                .to_string();
            let stderr = String::from_utf8_lossy(&stderr_buf)
                .replace(&marker, "")
                .trim()
                .to_string();

            Ok((stdout, stderr))
        } else {
            anyhow::bail!("Terminal not found")
        }
    }

    pub fn close_terminal(&self, terminal_id: &str) -> bool {
        self.terminals.remove(terminal_id).is_some()
    }
}
