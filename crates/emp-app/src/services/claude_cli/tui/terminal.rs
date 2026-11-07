use super::super::ClaudeCliError;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::mpsc,
    thread::JoinHandle,
};

pub(super) struct Terminal {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    master: Option<Box<dyn MasterPty + Send>>,
    reader: Option<JoinHandle<()>>,
    echo: Vec<u8>,
}

impl Terminal {
    pub(super) fn spawn(command: CommandBuilder) -> Result<Self, ClaudeCliError> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 50,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| ClaudeCliError::Failure("claude_cli_terminal_unavailable"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_terminal_unavailable"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_terminal_unavailable"))?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_spawn_failed"))?;
        drop(pair.slave);
        let (sender, output) = mpsc::sync_channel(64);
        let reader = std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                if let Err(mpsc::TrySendError::Disconnected(_)) =
                    sender.try_send(bytes[..count].to_vec())
                {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            writer,
            output,
            master: Some(pair.master),
            reader: Some(reader),
            echo: Vec::new(),
        })
    }

    pub(super) fn write(&mut self, bytes: &[u8]) -> Result<(), ClaudeCliError> {
        self.writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush())
            .map_err(|_| ClaudeCliError::Failure("claude_cli_input_failed"))
    }

    pub(super) fn pump(&mut self, marker: &[u8]) -> Result<bool, ClaudeCliError> {
        let mut echoed = false;
        while let Ok(bytes) = self.output.try_recv() {
            if bytes.windows(4).any(|part| part == b"\x1b[6n") {
                self.write(b"\x1b[1;1R")?;
            }
            if bytes.windows(3).any(|part| part == b"\x1b[c") {
                self.write(b"\x1b[?1;2c")?;
            }
            self.echo.extend_from_slice(&bytes);
            echoed |= self.echo.windows(marker.len()).any(|part| part == marker);
            if self.echo.len() > 128 {
                self.echo.drain(..self.echo.len() - 128);
            }
        }
        if self
            .child
            .try_wait()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_process_failed"))?
            .is_some()
        {
            return Err(ClaudeCliError::Failure("claude_cli_process_failed"));
        }
        Ok(echoed)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            // The PTY child owns its session; no user's terminal group is targeted.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
