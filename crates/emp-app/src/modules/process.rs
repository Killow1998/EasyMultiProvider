use crate::process_tree::ProcessTree;
use emp_modules::{
    manifest::{HOST_API_V3, HostApi, ManifestError, ModuleManifest},
    peer::{CallError, Handlers, Peer, PeerHandle, Side},
};
use serde::Serialize;
use std::{
    fmt, io,
    io::BufReader,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};
use tokio::sync::oneshot;

#[cfg(unix)]
use crate::process_tree::pipes::{Reader, Writer, interruptible};
#[cfg(windows)]
use std::io::{PipeReader as Reader, PipeWriter as Writer};

pub struct ModuleProcess {
    connection: PeerHandle,
    tree: Arc<ProcessTree>,
    exit: oneshot::Receiver<io::Result<ExitStatus>>,
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
        command: Command,
        state_dir: &Path,
        handlers: Handlers,
        timeout: Duration,
    ) -> Result<(Self, ModuleManifest), StartError> {
        Self::start_with_connector(command, state_dir, handlers, timeout, Peer::connect).await
    }

    pub(crate) async fn start_with_connector(
        mut command: Command,
        state_dir: &Path,
        handlers: Handlers,
        timeout: Duration,
        connect: impl FnOnce(Side, BufReader<Reader>, Writer, Handlers) -> io::Result<Peer>,
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
        #[cfg(unix)]
        let (reader, writer, close_pipes) =
            interruptible(reader, writer).map_err(StartError::Io)?;
        ProcessTree::configure(
            &mut command,
            #[cfg(windows)]
            windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
        );
        command
            .stdin(module_stdin)
            .stdout(module_stdout)
            .stderr(Stdio::null());
        let (monitor, start_monitor) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        thread::Builder::new()
            .name("emp-module-monitor".into())
            .spawn(move || {
                if let Ok(wait) = start_monitor.recv() {
                    wait();
                }
            })
            .map_err(StartError::Io)?;
        let mut child = command.spawn().map_err(StartError::Io)?;
        drop(command);
        #[cfg(windows)]
        use std::os::windows::io::AsRawHandle;
        let tree = match ProcessTree::attach(
            child.id(),
            #[cfg(windows)]
            child.as_raw_handle(),
        ) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(StartError::Io(error));
            }
        };
        let peer = match connect(Side::Host, BufReader::new(reader), writer, handlers) {
            Ok(peer) => peer,
            Err(error) => {
                tree.request_stop();
                let _ = tree.wait(&mut child);
                return Err(StartError::Io(error));
            }
        };
        let connection = peer.handle();
        let tree = Arc::new(tree);
        let process_tree = tree.clone();
        let (finished, exit) = oneshot::channel();
        monitor
            .send(Box::new(move || {
                let status = process_tree.wait(&mut child);
                #[cfg(unix)]
                drop(close_pipes);
                let _ = finished.send(peer.join().and(status));
            }))
            .expect("module monitor is waiting for the child");
        let process = Self {
            connection,
            tree,
            exit,
        };
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
        .map_err(|error| StartError::Io(io::Error::other(error)))
        .and_then(|result| result);
        match result {
            Ok(manifest) => Ok((process, manifest)),
            Err(error) => {
                let _ = process.stop().await;
                Err(error)
            }
        }
    }

    pub fn connection(&self) -> PeerHandle {
        self.connection.clone()
    }

    pub async fn stop(mut self) -> io::Result<ExitStatus> {
        self.tree.request_stop();
        (&mut self.exit).await.map_err(io::Error::other)?
    }
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        self.tree.request_stop();
    }
}
