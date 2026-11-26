pub use lsp_server::{ErrorCode, ResponseError};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, hash_map::Entry},
    fmt,
    io::{self, BufRead, Read, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
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
    InvalidFrame,
    Remote(ResponseError),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("连接已断开"),
            Self::Cancelled => f.write_str("请求已取消"),
            Self::TimedOut => f.write_str("请求超时"),
            Self::InvalidFrame => f.write_str("无法编码有效的 JSON-RPC 帧"),
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
    writer: Option<mpsc::Sender<Outgoing>>,
    closed: mpsc::Sender<io::Result<()>>,
    on_disconnect: Option<Box<dyn FnOnce() + Send>>,
    next_id: u64,
    pending: HashMap<RequestId, PendingReply>,
    incoming: HashMap<RequestId, CancellationToken>,
    draining_replies: bool,
}

enum Outgoing {
    Request(RequestId, Frame),
    Control(Frame),
}

struct Frame {
    header: String,
    body: Vec<u8>,
}

impl Frame {
    fn encode(message: &Message) -> io::Result<Self> {
        #[derive(Serialize)]
        struct JsonRpc<'a> {
            jsonrpc: &'static str,
            #[serde(flatten)]
            message: &'a Message,
        }

        let body = serde_json::to_vec(&JsonRpc {
            jsonrpc: "2.0",
            message,
        })?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        if header.len() + body.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid frame length",
            ));
        }
        Ok(Self { header, body })
    }
}

impl Connection {
    fn writer(&self) -> Result<&mpsc::Sender<Outgoing>, CallError> {
        self.writer.as_ref().ok_or(CallError::Disconnected)
    }

    fn send_control(&self, frame: Frame) -> Result<(), CallError> {
        self.writer()?
            .send(Outgoing::Control(frame))
            .map_err(|_| CallError::Disconnected)
    }

    fn disconnect(mut self, result: io::Result<()>) {
        for (_, pending) in self.pending.drain() {
            pending.reply.complete(Err(CallError::Disconnected));
        }
        self.cancel_incoming();
        self.signal_closed(result);
    }

    fn signal_closed(&mut self, result: io::Result<()>) {
        if let Some(notify) = self.on_disconnect.take() {
            notify();
        }
        let _ = self.closed.send(result);
    }

    fn cancel_incoming(&self) {
        for token in self.incoming.values() {
            token.cancel();
        }
    }
}

struct PendingReply {
    reply: Arc<Reply>,
    sent: bool,
}

#[derive(Default)]
struct Reply {
    result: Mutex<Option<CallResult>>,
    ready: Condvar,
}

impl Reply {
    fn complete(&self, result: CallResult) {
        *self.result.lock().unwrap() = Some(result);
        self.ready.notify_one();
    }
}

#[derive(Clone)]
pub struct PeerHandle {
    side: Side,
    connection: Arc<Mutex<Option<Connection>>>,
}

pub struct Peer {
    handle: PeerHandle,
    closed: mpsc::Receiver<io::Result<()>>,
    threads: Option<(thread::JoinHandle<()>, thread::JoinHandle<()>)>,
}

impl Peer {
    pub fn connect(
        side: Side,
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        handlers: Handlers,
    ) -> io::Result<Self> {
        Self::connect_inner(side, reader, writer, handlers, |_| {}, || {})
    }

    #[cfg(feature = "test-support")]
    pub fn connect_observed(
        side: Side,
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        handlers: Handlers,
        reader_exiting: impl FnOnce(Option<io::ErrorKind>) + Send + 'static,
        writer_exiting: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        Self::connect_inner(
            side,
            reader,
            writer,
            handlers,
            reader_exiting,
            writer_exiting,
        )
    }

    fn connect_inner(
        side: Side,
        mut reader: impl BufRead + Send + 'static,
        mut writer: impl Write + Send + 'static,
        handlers: Handlers,
        reader_exiting: impl FnOnce(Option<io::ErrorKind>) + Send + 'static,
        writer_exiting: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        let (send, frames) = mpsc::channel();
        let (close, closed) = mpsc::channel();
        let handle = PeerHandle {
            side,
            connection: Arc::new(Mutex::new(Some(Connection {
                writer: Some(send),
                closed: close,
                on_disconnect: None,
                next_id: 1,
                pending: HashMap::new(),
                incoming: HashMap::new(),
                draining_replies: false,
            }))),
        };
        let output = handle.clone();
        let writer = thread::Builder::new()
            .name("emp-peer-writer".into())
            .spawn(move || {
                output.write_loop(&mut writer, frames);
                writer_exiting();
            })?;
        let input = handle.clone();
        let reader = thread::Builder::new()
            .name("emp-peer-reader".into())
            .spawn(move || {
                let result = input.read_loop(&mut reader, &handlers);
                reader_exiting(result.as_ref().err().map(io::Error::kind));
                input.disconnect(result);
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                handle.close();
                let _ = writer.join();
                return Err(error);
            }
        };
        Ok(Self {
            handle,
            closed,
            threads: Some((reader, writer)),
        })
    }

    pub fn handle(&self) -> PeerHandle {
        self.handle.clone()
    }

    pub fn wait(self) -> io::Result<()> {
        self.closed.recv().map_err(io::Error::other)?
    }

    pub fn join(mut self) -> io::Result<()> {
        let (reader, writer) = self.threads.take().unwrap();
        let read = reader.join();
        let write = writer.join();
        read.and(write)
            .map_err(|_| io::Error::other("peer I/O thread panicked"))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let mut state = self.handle.connection.lock().unwrap();
        if let Some(connection) = state.take_if(|connection| connection.writer.is_some()) {
            connection.disconnect(Ok(()));
        } else if let Some(connection) = state.as_mut() {
            connection.draining_replies = true;
            connection.cancel_incoming();
        }
    }
}

impl PeerHandle {
    pub fn start_request(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<PendingRequest, CallError> {
        self.prepare_request(method, params)?.enqueue(None)
    }

    pub fn prepare_request(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<PreparedRequest, CallError> {
        let method = method.into();
        let (frame, id) = self.prepare_outgoing(move |connection| {
            let id = RequestId::from(format!("{}{}", self.side.prefix(), connection.next_id));
            connection.next_id += 1;
            let request = Request {
                id: id.clone(),
                method,
                params,
            };
            (request.into(), id)
        })?;
        Ok(PreparedRequest {
            peer: self.clone(),
            id,
            frame,
        })
    }

    pub fn call(&self, method: impl Into<String>, params: Value, timeout: Duration) -> CallResult {
        let started = Instant::now();
        let deadline = started.checked_add(timeout);
        self.prepare_request(method, params)?
            .enqueue(deadline)?
            .wait(timeout.saturating_sub(started.elapsed()))
    }

    pub fn notify(&self, method: impl Into<String>, params: Value) -> Result<(), CallError> {
        let notification = Notification {
            method: method.into(),
            params,
        };
        let (frame, ()) = self.prepare_outgoing(|_| (notification.into(), ()))?;
        let state = self.connection.lock().unwrap();
        state
            .as_ref()
            .ok_or(CallError::Disconnected)?
            .send_control(frame)
    }

    fn prepare_outgoing<T>(
        &self,
        build: impl FnOnce(&mut Connection) -> (Message, T),
    ) -> Result<(Frame, T), CallError> {
        let (message, metadata) = {
            let mut state = self.connection.lock().unwrap();
            let connection = state.as_mut().ok_or(CallError::Disconnected)?;
            connection.writer()?;
            build(connection)
        };
        let frame = Frame::encode(&message).or_else(|error| match message {
            Message::Response(response) => Frame::encode(
                &Response::new_err(
                    response.id,
                    ErrorCode::InternalError as i32,
                    "invalid frame length".into(),
                )
                .into(),
            ),
            _ => Err(error),
        });
        Ok((frame.map_err(|_| CallError::InvalidFrame)?, metadata))
    }

    fn send_response(&self, response: Response) {
        let id = response.id.clone();
        let prepared = self.prepare_outgoing(|_| (response.into(), ()));
        if let Some(connection) = self.connection.lock().unwrap().as_mut() {
            connection.incoming.remove(&id);
            if let Ok((frame, ())) = prepared {
                let _ = connection.send_control(frame);
            }
        }
    }

    pub fn close(&self) {
        self.disconnect(Ok(()));
    }

    pub fn on_disconnect(&self, notify: impl FnOnce() + Send + 'static) {
        let mut state = self.connection.lock().unwrap();
        if let Some(connection) = state
            .as_mut()
            .filter(|connection| connection.writer.is_some())
        {
            assert!(connection.on_disconnect.replace(Box::new(notify)).is_none());
        } else {
            notify();
        }
    }

    fn disconnect(&self, result: io::Result<()>) {
        let mut state = self.connection.lock().unwrap();
        if let Some(connection) = state.take() {
            connection.disconnect(result);
        }
    }

    fn write_loop(&self, writer: &mut impl Write, frames: mpsc::Receiver<Outgoing>) {
        for message in frames {
            if self.connection.lock().unwrap().is_none() {
                break;
            }
            let (request_id, frame) = match message {
                Outgoing::Request(id, frame) => (Some(id), frame),
                Outgoing::Control(frame) => (None, frame),
            };
            if let Err(error) = writer
                .write_all(frame.header.as_bytes())
                .and_then(|()| writer.write_all(&frame.body))
                .and_then(|()| writer.flush())
            {
                let mut state = self.connection.lock().unwrap();
                if let Some(connection) = state.as_mut() {
                    connection.writer.take();
                    connection.pending.retain(|_, pending| {
                        if !pending.sent {
                            pending.reply.complete(Err(CallError::Disconnected));
                        }
                        pending.sent
                    });
                    connection.signal_closed(Err(error));
                }
                break;
            }
            if let Some(id) = request_id
                && let Some(connection) = self.connection.lock().unwrap().as_mut()
                && let Some(pending) = connection.pending.get_mut(&id)
            {
                pending.sent = true;
            }
        }
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
                    if let Some(pending) = connection.pending.remove(&response.id) {
                        pending
                            .reply
                            .complete(response.response_result.map_err(CallError::Remote));
                    }
                }
                Message::Notification(notification) if notification.method == CANCEL_REQUEST => {
                    #[derive(serde::Deserialize)]
                    struct CancelParams {
                        id: RequestId,
                    }
                    let params: CancelParams = serde_json::from_value(notification.params)?;
                    self.side.opposite().check_id(&params.id)?;
                    if !connection.draining_replies
                        && let Some(token) = connection.incoming.get(&params.id)
                    {
                        token.cancel();
                    }
                }
                Message::Request(request) => {
                    self.side.opposite().check_id(&request.id)?;
                    let Entry::Vacant(entry) = connection.incoming.entry(request.id.clone()) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate request ID",
                        ));
                    };
                    if connection.draining_replies {
                        continue;
                    }
                    let Some(handler) = handlers.0.get(&request.method).cloned() else {
                        drop(state);
                        self.send_response(Response::new_err(
                            request.id,
                            ErrorCode::MethodNotFound as i32,
                            request.method,
                        ));
                        continue;
                    };
                    let token = CancellationToken::default();
                    entry.insert(token.clone());
                    drop(state);
                    let peer = self.clone();
                    thread::Builder::new().spawn(move || {
                        let mut reply = HandlerReply {
                            peer,
                            id: request.id,
                            result: None,
                        };
                        reply.result = Some(handler(request.params, token, &reply.peer));
                    })?;
                }
                Message::Notification(notification) => {
                    if connection.draining_replies {
                        continue;
                    }
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

struct HandlerReply {
    peer: PeerHandle,
    id: RequestId,
    result: Option<Result<Value, ResponseError>>,
}

impl Drop for HandlerReply {
    fn drop(&mut self) {
        let response_result = self.result.take().unwrap_or_else(|| {
            Err(ResponseError {
                code: ErrorCode::InternalError as i32,
                message: "request handler panicked".into(),
                data: None,
            })
        });
        self.peer.send_response(Response {
            id: self.id.clone(),
            response_result,
        });
    }
}

pub struct PreparedRequest {
    peer: PeerHandle,
    id: RequestId,
    frame: Frame,
}

impl PreparedRequest {
    pub fn enqueue(self, deadline: Option<Instant>) -> Result<PendingRequest, CallError> {
        let Self { peer, id, frame } = self;
        let mut state = peer.connection.lock().unwrap();
        let connection = state.as_mut().ok_or(CallError::Disconnected)?;
        let writer = connection.writer()?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(CallError::TimedOut);
        }
        writer
            .send(Outgoing::Request(id.clone(), frame))
            .map_err(|_| CallError::Disconnected)?;
        let reply = Arc::new(Reply::default());
        connection.pending.insert(
            id.clone(),
            PendingReply {
                reply: reply.clone(),
                sent: false,
            },
        );
        drop(state);
        Ok(PendingRequest {
            reply,
            canceller: RequestCanceller { peer, id },
        })
    }
}

pub struct PendingRequest {
    reply: Arc<Reply>,
    canceller: RequestCanceller,
}

impl PendingRequest {
    pub fn canceller(&self) -> RequestCanceller {
        self.canceller.clone()
    }

    pub fn wait(self, timeout: Duration) -> CallResult {
        self.wait_inner(timeout, || {})
    }

    #[cfg(feature = "test-support")]
    pub fn wait_registered(self, timeout: Duration, registered: impl FnOnce()) -> CallResult {
        self.wait_inner(timeout, registered)
    }

    fn wait_inner(self, timeout: Duration, registered: impl FnOnce()) -> CallResult {
        let mut registered = Some(registered);
        let result = self.reply.result.lock().unwrap();
        let (mut result, _) = self
            .reply
            .ready
            .wait_timeout_while(result, timeout, |result| {
                if result.is_some() {
                    return false;
                }
                if let Some(registered) = registered.take() {
                    registered();
                }
                true
            })
            .unwrap();
        if let Some(result) = result.take() {
            return result;
        }
        drop(result);
        self.canceller.finish(CallError::TimedOut);
        self.reply.result.lock().unwrap().take().unwrap()
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
        if !self
            .peer
            .connection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|connection| connection.pending.contains_key(&self.id))
        {
            return;
        }
        let notification = Notification {
            method: CANCEL_REQUEST.into(),
            params: json!({"id": self.id}),
        };
        let prepared = self.peer.prepare_outgoing(|_| (notification.into(), ()));
        let mut state = self.peer.connection.lock().unwrap();
        if let Some(connection) = state.as_mut()
            && let Some(pending) = connection.pending.remove(&self.id)
        {
            pending.reply.complete(Err(error));
            if let Ok((frame, ())) = prepared {
                let _ = connection.send_control(frame);
            }
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
    let Some(length) = length.filter(|&length| length <= MAX_FRAME_BYTES - headers.len()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame length",
        ));
    };
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| io::Error::new(error.kind(), "cannot read peer frame"))?;
    crate::strict_json::from_slice(&body)
        .and_then(serde_json::from_value)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cannot read peer frame"))
}
