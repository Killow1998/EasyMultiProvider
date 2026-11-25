use emp_modules::{
    manifest::{HOST_API_V3, HostApi, ManifestError, ModuleManifest},
    peer::{CallError, Handlers, Peer, PeerHandle, Side},
};
use serde::Serialize;
use std::{
    fmt, io,
    io::BufReader,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::sync::oneshot;

pub struct ModuleProcess {
    peer: Peer,
    shutdown: oneshot::Sender<()>,
}

#[derive(Debug)]
pub enum StartError {
    Io(io::Error),
    Handshake(CallError),
    InvalidManifest(emp_modules::manifest::ManifestDecodeError),
    Manifest(Vec<ManifestError>),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "无法启动模组进程: {error}"),
            Self::Handshake(error) => write!(f, "模组初始化失败: {error}"),
            Self::InvalidManifest(error) => write!(f, "无法读取模组 manifest: {error}"),
            Self::Manifest(errors) => {
                f.write_str("模组 manifest 校验失败")?;
                for error in errors {
                    write!(f, "\n{error}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for StartError {}

impl ModuleProcess {
    pub async fn start(
        mut command: Command,
        state_dir: &Path,
        handlers: Handlers,
        timeout: Duration,
    ) -> Result<(Self, ModuleManifest), StartError> {
        #[derive(Serialize)]
        struct Initialize<'a> {
            host_api: HostApi,
            state_dir: &'a Path,
        }

        let params = serde_json::to_value(Initialize {
            host_api: HOST_API_V3,
            state_dir,
        })
        .map_err(|error| StartError::Io(io::Error::new(io::ErrorKind::InvalidInput, error)))?;
        let (module_stdin, writer) = io::pipe().map_err(StartError::Io)?;
        let (reader, module_stdout) = io::pipe().map_err(StartError::Io)?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        command
            .stdin(module_stdin)
            .stdout(module_stdout)
            .stderr(Stdio::null());
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .map_err(StartError::Io)?;
        let peer = match Peer::connect(Side::Host, BufReader::new(reader), writer, handlers) {
            Ok(peer) => peer,
            Err(error) => {
                let _ = child.kill().await;
                return Err(StartError::Io(error));
            }
        };
        let connection = peer.handle();
        let (shutdown, stopping) = oneshot::channel();
        let exit = tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = stopping => { let _ = child.kill().await; }
            }
            connection.close();
        });
        let process = Self { peer, shutdown };
        let connection = process.connection();
        let result = tokio::task::spawn_blocking(move || {
            let value = connection
                .call("module.initialize", params, timeout)
                .map_err(StartError::Handshake)?;
            let manifest =
                ModuleManifest::from_value(value).map_err(StartError::InvalidManifest)?;
            manifest
                .validate(HOST_API_V3)
                .map_err(StartError::Manifest)?;
            Ok(manifest)
        })
        .await
        .map_err(|error| StartError::Io(io::Error::other(error)))?;
        match result {
            Ok(manifest) => Ok((process, manifest)),
            Err(error) => {
                let _ = process.shutdown.send(());
                let _ = exit.await;
                Err(error)
            }
        }
    }

    pub fn connection(&self) -> PeerHandle {
        self.peer.handle()
    }
}
