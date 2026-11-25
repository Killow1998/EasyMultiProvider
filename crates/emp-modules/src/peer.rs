pub use lsp_server::{ErrorCode, ResponseError};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, hash_map::Entry},
    fmt,
    io::{self, BufRead, Cursor, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const CANCEL_REQUEST: &str = "$/cancelRequest";

#[derive(Clone, Copy)]
pub enum Side {
    Host,
    Module,
}

impl Side {
    fn prefix(self) -> &'static str {
        match self {
            Self::Host => "h:",
            Self::Module => "m:",
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Host => Self::Module,
            Self::Module => Self::Host,
        }
    }

    fn check_id(self, id: &RequestId) -> io::Result<()> {
        let id = serde_json::to_value(id)?;
        if id.as_str().is_some_and(|id| {
            id.strip_prefix(self.prefix())
                .is_some_and(|number| number.parse::<u64>().is_ok())
        }) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid request ID",
            ))
        }
    }
}

#[derive(Debug)]
pub enum CallError {
    Disconnected,
    Cancelled,
    TimedOut,
    Remote(ResponseError),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("连接已断开"),
            Self::Cancelled => f.write_str("请求已取消"),
            Self::TimedOut => f.write_str("请求超时"),
            Self::Remote(error) => write!(f, "JSON-RPC {}: {}", error.code, error.message),
        }
    }
}

impl std::error::Error for CallError {}

type CallResult = Result<Value, CallError>;
type Handler =
    dyn Fn(Value, CancellationToken, &PeerHandle) -> Result<Value, ResponseError> + Send + Sync;

#[derive(Default)]
pub struct Handlers(HashMap<String, Arc<Handler>>);

impl Handlers {
    pub fn register(
        &mut self,
        method: impl Into<String>,
        handler: impl Fn(Value, CancellationToken, &PeerHandle) -> Result<Value, ResponseError>
        + Send
        + Sync
        + 'static,
    ) {
        self.0.insert(method.into(), Arc::new(handler));
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

struct Connection {
    writer: mpsc::Sender<Message>,
    closed: mpsc::Sender<io::Result<()>>,
    next_id: u64,
    pending: HashMap<RequestId, mpsc::Sender<CallResult>>,
    incoming: HashMap<RequestId, CancellationToken>,
}

#[derive(Clone)]
pub struct PeerHandle {
    side: Side,
    connection: Arc<Mutex<Option<Connection>>>,
}

pub struct Peer {
    handle: PeerHandle,
    closed: mpsc::Receiver<io::Result<()>>,
}

impl Peer {
    pub fn connect(
        side: Side,
        mut reader: impl BufRead + Send + 'static,
        mut writer: impl Write + Send + 'static,
        handlers: Handlers,
    ) -> io::Result<Self> {
        let (send, frames) = mpsc::channel();
        let (close, closed) = mpsc::channel();
        let handle = PeerHandle {
            side,
            connection: Arc::new(Mutex::new(Some(Connection {
                writer: send,
                closed: close,
                next_id: 1,
                pending: HashMap::new(),
                incoming: HashMap::new(),
            }))),
        };
        let output = handle.clone();
        thread::Builder::new()
            .name("emp-peer-writer".into())
            .spawn(move || {
                let result = output.write_loop(&mut writer, frames);
                output.disconnect(result);
            })?;
        let input = handle.clone();
        thread::Builder::new()
            .name("emp-peer-reader".into())
            .spawn(move || {
                let result = input.read_loop(&mut reader, &handlers);
                input.disconnect(result);
            })
            .inspect_err(|_| handle.close())?;
        Ok(Self { handle, closed })
    }

    pub fn handle(&self) -> PeerHandle {
        self.handle.clone()
    }

    pub fn wait(self) -> io::Result<()> {
        self.closed.recv().map_err(io::Error::other)?
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.handle.close();
    }
}

impl PeerHandle {
    pub fn start_request(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<PendingRequest, CallError> {
        let (waiter, reply) = mpsc::channel();
        let method = method.into();
        let mut state = self.connection.lock().unwrap();
        let connection = state.as_mut().ok_or(CallError::Disconnected)?;
        let id = RequestId::from(format!("{}{}", self.side.prefix(), connection.next_id));
        connection.next_id += 1;
        connection
            .writer
            .send(
                Request {
                    id: id.clone(),
                    method,
                    params,
                }
                .into(),
            )
            .map_err(|_| CallError::Disconnected)?;
        connection.pending.insert(id.clone(), waiter);
        Ok(PendingRequest {
            reply,
            canceller: RequestCanceller {
                peer: self.clone(),
                id,
            },
        })
    }

    pub fn call(&self, method: impl Into<String>, params: Value, timeout: Duration) -> CallResult {
        self.start_request(method, params)?.wait(timeout)
    }

    pub fn notify(&self, method: impl Into<String>, params: Value) -> Result<(), CallError> {
        let notification = Notification {
            method: method.into(),
            params,
        };
        let state = self.connection.lock().unwrap();
        let connection = state.as_ref().ok_or(CallError::Disconnected)?;
        connection
            .writer
            .send(notification.into())
            .map_err(|_| CallError::Disconnected)
    }

    pub fn close(&self) {
        self.disconnect(Ok(()));
    }

    fn disconnect(&self, result: io::Result<()>) {
        let connection = self.connection.lock().unwrap().take();
        if let Some(connection) = connection {
            for waiter in connection.pending.into_values() {
                let _ = waiter.send(Err(CallError::Disconnected));
            }
            for token in connection.incoming.into_values() {
                token.cancel();
            }
            let _ = connection.closed.send(result);
        }
    }

    fn write_loop(
        &self,
        writer: &mut impl Write,
        frames: mpsc::Receiver<Message>,
    ) -> io::Result<()> {
        let mut frame = Vec::new();
        for message in frames {
            if self.connection.lock().unwrap().is_none() {
                break;
            }
            frame.clear();
            message.write(&mut frame)?;
            if frame.len() > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid frame length",
                ));
            }
            writer.write_all(&frame)?;
            writer.flush()?;
        }
        Ok(())
    }

    fn read_loop(&self, reader: &mut impl BufRead, handlers: &Handlers) -> io::Result<()> {
        while let Some(message) = read_frame(reader)? {
            let mut state = self.connection.lock().unwrap();
            let Some(connection) = state.as_mut() else {
                break;
            };
            match message {
                Message::Response(response) => {
                    self.side.check_id(&response.id)?;
                    if let Some(waiter) = connection.pending.remove(&response.id) {
                        let _ = waiter.send(response.response_result.map_err(CallError::Remote));
                    }
                }
                Message::Notification(notification) if notification.method == CANCEL_REQUEST => {
                    #[derive(serde::Deserialize)]
                    struct CancelParams {
                        id: RequestId,
                    }
                    let params: CancelParams = serde_json::from_value(notification.params)?;
                    self.side.opposite().check_id(&params.id)?;
                    if let Some(token) = connection.incoming.get(&params.id) {
                        token.cancel();
                    }
                }
                Message::Request(request) => {
                    self.side.opposite().check_id(&request.id)?;
                    let Some(handler) = handlers.0.get(&request.method).cloned() else {
                        let _ = connection.writer.send(
                            Response::new_err(
                                request.id,
                                ErrorCode::MethodNotFound as i32,
                                request.method,
                            )
                            .into(),
                        );
                        continue;
                    };
                    let Entry::Vacant(entry) = connection.incoming.entry(request.id.clone()) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate request ID",
                        ));
                    };
                    let token = CancellationToken::default();
                    entry.insert(token.clone());
                    drop(state);
                    let peer = self.clone();
                    thread::Builder::new().spawn(move || {
                        let response_result = handler(request.params, token, &peer);
                        if let Some(connection) = peer.connection.lock().unwrap().as_mut() {
                            connection.incoming.remove(&request.id);
                            let _ = connection.writer.send(
                                Response {
                                    id: request.id,
                                    response_result,
                                }
                                .into(),
                            );
                        }
                    })?;
                }
                Message::Notification(notification) => {
                    drop(state);
                    if let Some(handler) = handlers.0.get(&notification.method).cloned() {
                        let peer = self.clone();
                        thread::Builder::new().spawn(move || {
                            let _ =
                                handler(notification.params, CancellationToken::default(), &peer);
                        })?;
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct PendingRequest {
    reply: mpsc::Receiver<CallResult>,
    canceller: RequestCanceller,
}

impl PendingRequest {
    pub fn canceller(&self) -> RequestCanceller {
        self.canceller.clone()
    }

    pub fn wait(self, timeout: Duration) -> CallResult {
        match self.reply.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CallError::Disconnected),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.canceller.finish(CallError::TimedOut);
                self.reply.recv().unwrap_or(Err(CallError::Disconnected))
            }
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.canceller.cancel();
    }
}

#[derive(Clone)]
pub struct RequestCanceller {
    peer: PeerHandle,
    id: RequestId,
}

impl RequestCanceller {
    pub fn cancel(&self) {
        self.finish(CallError::Cancelled);
    }

    fn finish(&self, error: CallError) {
        let mut state = self.peer.connection.lock().unwrap();
        if let Some(connection) = state.as_mut()
            && let Some(waiter) = connection.pending.remove(&self.id)
        {
            let _ = waiter.send(Err(error));
            let _ = connection
                .writer
                .send(Notification::new(CANCEL_REQUEST.into(), json!({"id": self.id})).into());
        }
    }
}

fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Message>> {
    let mut headers = Vec::new();
    let mut length = None;
    loop {
        let start = headers.len();
        let bytes = reader
            .by_ref()
            .take((MAX_FRAME_BYTES - start) as u64)
            .read_until(b'\n', &mut headers)?;
        if start == 0 && bytes == 0 {
            return Ok(None);
        }
        let line = headers[start..]
            .strip_suffix(b"\r\n")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid frame header"))?;
        if line.is_empty() {
            break;
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid frame header"))?;
        let (name, value) = line
            .split_once(": ")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid frame header"))?;
        if name.eq_ignore_ascii_case("Content-Length") {
            length = match length {
                None => value.parse::<usize>().ok(),
                Some(_) => None,
            };
            if length.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Content-Length",
                ));
            }
        }
    }
    if length.is_none_or(|length| length > MAX_FRAME_BYTES - headers.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame length",
        ));
    }
    Message::read(&mut Cursor::new(headers).chain(reader))
        .map_err(|error| io::Error::new(error.kind(), "cannot read peer frame"))
}
