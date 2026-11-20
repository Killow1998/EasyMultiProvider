use emp_modules::{Command, Hello, Module, Reply};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command as Process, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    time::Duration,
};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct Worker {
    pub(super) module_version: String,
    pub(super) panel: String,
    child: Child,
    commands: SyncSender<Vec<u8>>,
    replies: Receiver<Vec<u8>>,
}

impl Worker {
    pub(super) fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub(super) fn open(path: &Path, module: Module) -> Result<Self, &'static str> {
        let mut process = Process::new(path);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let mut child = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "module_start_failed")?;
        let mut writer = child.stdin.take().expect("piped stdin");
        let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        let (sender, replies) = mpsc::sync_channel(1);
        let (commands, input) = mpsc::sync_channel::<Vec<u8>>(1);
        let mut worker = Self {
            module_version: String::new(),
            panel: String::new(),
            child,
            commands,
            replies,
        };
        std::thread::Builder::new()
            .name(format!("empmod-{}", module.id()))
            .spawn(move || {
                loop {
                    let mut line = Vec::new();
                    if reader
                        .by_ref()
                        .take(MAX_FRAME_BYTES as u64)
                        .read_until(b'\n', &mut line)
                        .is_err()
                        || line.last() != Some(&b'\n')
                        || sender.send(line).is_err()
                    {
                        break;
                    }
                    let Ok(message) = input.recv() else { break };
                    if writer.write_all(&message).is_err() {
                        break;
                    }
                }
            })
            .map_err(|_| "module_start_failed")?;
        let line = worker
            .replies
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "module_start_failed")?;
        let hello: Hello = serde_json::from_slice(&line).map_err(|_| "invalid_module_handshake")?;
        if hello.module != module
            || hello.protocol != emp_modules::PROTOCOL
            || hello.host_api != Some(emp_modules::HOST_API)
        {
            return Err("incompatible_module");
        }
        worker.module_version = hello
            .module_version
            .filter(|version| !version.is_empty())
            .ok_or("invalid_module_handshake")?;
        let panel = worker
            .call(&Command::ReadPanel)?
            .map_err(|_| "invalid_module_panel")?;
        worker.panel = serde_json::from_value(panel).map_err(|_| "invalid_module_panel")?;
        if worker.panel.is_empty() {
            return Err("invalid_module_panel");
        }
        Ok(worker)
    }

    pub(super) fn call(
        &mut self,
        command: &Command,
    ) -> Result<Result<Value, String>, &'static str> {
        let mut message = serde_json::to_vec(command).map_err(|_| "invalid_module_command")?;
        if message.len() >= MAX_FRAME_BYTES {
            return Ok(Err("invalid_module_command".into()));
        }
        message.push(b'\n');
        self.commands
            .try_send(message)
            .map_err(|_| "module_stopped")?;
        let line = self
            .replies
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "module_unresponsive")?;
        let reply: Reply = serde_json::from_slice(&line).map_err(|_| "invalid_module_reply")?;
        Ok(match reply.error {
            Some(error) => Err(error),
            None => Ok(reply.value),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
