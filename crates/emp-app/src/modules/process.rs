use crate::{process_tree::ProcessTree, util::random_hex};
use axum::http::HeaderValue;
use emp_modules::{
    handshake::{INITIALIZE, Initialize, Initialized},
    manifest::{HOST_API_V3, ManifestError, ModuleManifest},
    peer::{CallError, Handlers, Peer, PeerHandle, Side},
};
use std::{
    fmt, io,
    io::BufReader,
    num::NonZeroU16,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};
use tokio::sync::{oneshot, watch};

#[cfg(unix)]
use crate::process_tree::pipes::{Reader, Writer, interruptible};
#[cfg(windows)]
use std::io::{PipeReader as Reader, PipeWriter as Writer};

pub struct ModuleProcess {
    connection: PeerHandle,
    pub(crate) data_endpoint: Option<DataEndpoint>,
    generation: Arc<Generation>,
    exit: oneshot::Receiver<io::Result<ExitStatus>>,
}

struct Generation {
    tree: ProcessTree,
    disconnected: watch::Sender<bool>,
}

impl Generation {
    fn invalidate(&self) {
        self.disconnected.send_if_modified(|closed| {
            if *closed {
                return false;
            }
            *closed = true;
            self.tree.request_stop();
            true
        });
    }
}

#[derive(Clone)]
pub(crate) struct DataEndpoint {
    pub(crate) port: NonZeroU16,
    pub(crate) authorization: HeaderValue,
    pub(crate) disconnected: watch::Receiver<bool>,
}

#[derive(Debug)]
pub enum StartError {
    Io(io::Error),
    Handshake(CallError),
    InvalidManifest(serde_json::Error),
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
        Self::start_with_connector(command, state_dir, handlers, timeout, Peer::connect, || {})
            .await
    }

    pub(crate) async fn start_with_connector(
        mut command: Command,
        state_dir: &Path,
        handlers: Handlers,
        timeout: Duration,
        connect: impl FnOnce(Side, BufReader<Reader>, Writer, Handlers) -> io::Result<Peer>,
        process_exited: impl FnOnce() + Send + 'static,
    ) -> Result<(Self, ModuleManifest), StartError> {
        let initialize = Initialize {
            host_api: HOST_API_V3,
            state_dir: state_dir.to_path_buf(),
            callback_port: None,
            module_bearer: random_hex(32)
                .map_err(|error| StartError::Io(io::Error::other(error)))?,
            core_bearer: random_hex(32).map_err(|error| StartError::Io(io::Error::other(error)))?,
        };
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", initialize.module_bearer))
                .expect("hex bearer is a valid header value");
        authorization.set_sensitive(true);
        let params = serde_json::to_value(initialize)
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
        let tree = match ProcessTree::attach(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(StartError::Io(error));
            }
        };
        let (signal, disconnected) = watch::channel(false);
        let generation = Arc::new(Generation {
            tree,
            disconnected: signal,
        });
        let peer = match connect(Side::Host, BufReader::new(reader), writer, handlers) {
            Ok(peer) => peer,
            Err(error) => {
                generation.invalidate();
                let _ = generation.tree.wait(&mut child);
                return Err(StartError::Io(error));
            }
        };
        let connection = peer.handle();
        let on_disconnect = generation.clone();
        connection.on_disconnect(move || on_disconnect.invalidate());
        let monitored = generation.clone();
        let (finished, exit) = oneshot::channel();
        monitor
            .send(Box::new(move || {
                let status = monitored.tree.wait(&mut child);
                monitored.invalidate();
                #[cfg(unix)]
                drop(close_pipes);
                process_exited();
                let _ = finished.send(peer.join().and(status));
            }))
            .expect("module monitor is waiting for the child");
        let mut process = Self {
            connection,
            data_endpoint: None,
            generation,
            exit,
        };
        let connection = process.connection();
        let result = tokio::task::spawn_blocking(move || {
            let value = connection
                .call(INITIALIZE, params, timeout)
                .map_err(StartError::Handshake)?;
            let initialized: Initialized =
                serde_json::from_value(value).map_err(StartError::InvalidManifest)?;
            initialized
                .manifest
                .validate(HOST_API_V3)
                .map_err(StartError::Manifest)?;
            Ok(initialized)
        })
        .await
        .map_err(|error| StartError::Io(io::Error::other(error)))
        .and_then(|result| result);
        match result {
            Ok(Initialized {
                manifest,
                data_port,
            }) => {
                process.data_endpoint = data_port.map(|port| DataEndpoint {
                    port,
                    authorization,
                    disconnected,
                });
                Ok((process, manifest))
            }
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
        self.generation.invalidate();
        (&mut self.exit).await.map_err(io::Error::other)?
    }
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        self.generation.invalidate();
    }
}
