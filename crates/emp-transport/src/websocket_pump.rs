//! Bounded, single-owner upstream WebSocket pump.

use crate::websocket::{ClientWebSocket, ClientWebSocketError};
use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::JoinHandle;
use std::time::Duration;

pub const DEFAULT_WEBSOCKET_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_PUMP_CHANNEL_CAPACITY: usize = 8;
pub const MAX_PUMP_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub enum WebSocketPoll<T> {
    Pending,
    Text(T),
    Ping(Vec<u8>),
    Closed { code: Option<u16> },
}

impl<T> fmt::Debug for WebSocketPoll<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("Pending"),
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Ping(_) => formatter.write_str("Ping(<redacted>)"),
            Self::Closed { code } => formatter
                .debug_struct("Closed")
                .field("code", code)
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum PumpCommand {
    Text(String),
    Close { code: u16, reason: String },
}

impl fmt::Debug for PumpCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Close { code, .. } => formatter
                .debug_struct("Close")
                .field("code", code)
                .field("reason", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum PumpEvent {
    Text(String),
    Closed { code: Option<u16> },
    Failure { status: u16 },
}

impl fmt::Debug for PumpEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Closed { code } => formatter
                .debug_struct("Closed")
                .field("code", code)
                .finish(),
            Self::Failure { status } => formatter
                .debug_struct("Failure")
                .field("status", status)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketPumpConfig {
    pub outbound_capacity: usize,
    pub inbound_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for WebSocketPumpConfig {
    fn default() -> Self {
        Self {
            outbound_capacity: DEFAULT_PUMP_CHANNEL_CAPACITY,
            inbound_capacity: DEFAULT_PUMP_CHANNEL_CAPACITY,
            max_message_bytes: DEFAULT_WEBSOCKET_MESSAGE_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{self, Read};

    enum ReadAction {
        Bytes(Vec<u8>),
        WouldBlock,
    }

    struct ScriptedReader {
        actions: VecDeque<ReadAction>,
        pending: VecDeque<u8>,
    }

    impl ScriptedReader {
        fn new(actions: impl IntoIterator<Item = ReadAction>) -> Self {
            Self {
                actions: actions.into_iter().collect(),
                pending: VecDeque::new(),
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.pending.is_empty() {
                match self.actions.pop_front() {
                    Some(ReadAction::Bytes(bytes)) => self.pending.extend(bytes),
                    Some(ReadAction::WouldBlock) | None => {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                }
            }
            let count = output.len().min(self.pending.len());
            for slot in &mut output[..count] {
                *slot = self.pending.pop_front().expect("pending byte exists");
            }
            Ok(count)
        }
    }

    #[test]
    fn frame_decoder_preserves_partial_header_and_payload_across_timeouts() {
        let mut decoder = FrameDecoder::new(32);
        let mut reader = ScriptedReader::new([
            ReadAction::Bytes(vec![0x81]),
            ReadAction::WouldBlock,
            ReadAction::Bytes(vec![5, b'h', b'e']),
            ReadAction::WouldBlock,
            ReadAction::Bytes(b"llo".to_vec()),
        ]);
        assert!(matches!(
            decoder.poll(&mut reader, false, false).unwrap(),
            FramePoll::Pending
        ));
        assert!(matches!(
            decoder.poll(&mut reader, false, false).unwrap(),
            FramePoll::Pending
        ));
        match decoder.poll(&mut reader, false, false).unwrap() {
            FramePoll::Message {
                opcode: 1,
                payload,
                compressed: false,
            } => assert_eq!(payload, b"hello"),
            _ => panic!("expected completed text frame"),
        }
    }

    #[test]
    fn frame_decoder_surfaces_ping_between_fragmented_text_frames() {
        let mut decoder = FrameDecoder::new(32);
        let mut reader = ScriptedReader::new([
            ReadAction::Bytes(vec![
                0x01, 3, b'h', b'e', b'l', 0x89, 1, b'p', 0x80, 2, b'l', b'o',
            ]),
            ReadAction::WouldBlock,
        ]);
        assert!(matches!(
            decoder.poll(&mut reader, false, false).unwrap(),
            FramePoll::Ping(payload) if payload == b"p"
        ));
        match decoder.poll(&mut reader, false, false).unwrap() {
            FramePoll::Message {
                opcode: 1,
                payload,
                compressed: false,
            } => assert_eq!(payload, b"hello"),
            _ => panic!("expected reassembled text message"),
        }
    }

    #[test]
    fn frame_decoder_rejects_oversize_message_from_header_before_body() {
        let mut decoder = FrameDecoder::new(4);
        let mut reader = ScriptedReader::new([ReadAction::Bytes(vec![0x81, 5])]);
        assert!(matches!(
            decoder.poll(&mut reader, false, false),
            Err(FrameDecodeError::TooLarge)
        ));
    }

    #[test]
    fn pump_config_uses_four_mib_default_and_payload_debug_is_redacted() {
        assert_eq!(
            WebSocketPumpConfig::default().max_message_bytes,
            DEFAULT_WEBSOCKET_MESSAGE_BYTES
        );
        assert_eq!(DEFAULT_WEBSOCKET_MESSAGE_BYTES, 4 * 1024 * 1024);
        assert!(!format!("{:?}", PumpCommand::Text("private".to_owned())).contains("private"));
        assert!(!format!("{:?}", PumpEvent::Text("private".to_owned())).contains("private"));
        assert!(!format!("{:?}", WebSocketPoll::Text("private".to_owned())).contains("private"));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameDecodeError {
    Protocol,
    TooLarge,
    Io,
}

pub(crate) enum FramePoll {
    Pending,
    Message {
        opcode: u8,
        payload: Vec<u8>,
        compressed: bool,
    },
    Ping(Vec<u8>),
    Closed {
        code: Option<u16>,
        payload: Vec<u8>,
    },
}

struct ParsedFrame {
    final_frame: bool,
    rsv1: bool,
    opcode: u8,
    payload: Vec<u8>,
}

pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
    message: Vec<u8>,
    message_opcode: Option<u8>,
    compressed: bool,
    max_message_bytes: usize,
}

impl FrameDecoder {
    pub(crate) fn new(max_message_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            message: Vec::new(),
            message_opcode: None,
            compressed: false,
            max_message_bytes,
        }
    }

    pub(crate) fn set_max_message_bytes(&mut self, maximum: usize) {
        self.max_message_bytes = maximum;
    }

    pub(crate) fn feed(&mut self, prefix: &[u8]) -> Result<(), FrameDecodeError> {
        let limit = self
            .max_message_bytes
            .checked_add(8192)
            .ok_or(FrameDecodeError::TooLarge)?;
        if self
            .buffer
            .len()
            .checked_add(prefix.len())
            .is_none_or(|length| length > limit)
        {
            return Err(FrameDecodeError::TooLarge);
        }
        self.buffer.extend_from_slice(prefix);
        Ok(())
    }

    pub(crate) fn poll<R: std::io::Read + ?Sized>(
        &mut self,
        stream: &mut R,
        expect_mask: bool,
        allow_compression: bool,
    ) -> Result<FramePoll, FrameDecodeError> {
        loop {
            if let Some(event) = self.parse_buffered(expect_mask, allow_compression)? {
                return Ok(event);
            }
            let mut chunk = [0u8; 8192];
            match stream.read(&mut chunk) {
                Ok(0) => {
                    return Ok(FramePoll::Closed {
                        code: None,
                        payload: Vec::new(),
                    });
                }
                Ok(count) => self.buffer.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(FramePoll::Pending);
                }
                Err(_) => return Err(FrameDecodeError::Io),
            }
        }
    }

    fn parse_buffered(
        &mut self,
        expect_mask: bool,
        allow_compression: bool,
    ) -> Result<Option<FramePoll>, FrameDecodeError> {
        loop {
            let Some(frame) = self.next_frame(expect_mask)? else {
                return Ok(None);
            };
            match frame.opcode {
                8 => {
                    let code = match frame.payload.len() {
                        0 => None,
                        1 => return Err(FrameDecodeError::Protocol),
                        _ => Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]])),
                    };
                    return Ok(Some(FramePoll::Closed {
                        code,
                        payload: frame.payload,
                    }));
                }
                9 => return Ok(Some(FramePoll::Ping(frame.payload))),
                10 => return Ok(Some(FramePoll::Pending)),
                1 | 2 => {
                    if self.message_opcode.is_some() {
                        return Err(FrameDecodeError::Protocol);
                    }
                    if frame.rsv1 && !allow_compression {
                        return Err(FrameDecodeError::Protocol);
                    }
                    self.message_opcode = Some(frame.opcode);
                    self.compressed = frame.rsv1;
                }
                0 => {
                    if self.message_opcode.is_none() || frame.rsv1 {
                        return Err(FrameDecodeError::Protocol);
                    }
                }
                _ => return Err(FrameDecodeError::Protocol),
            }
            if self
                .message
                .len()
                .checked_add(frame.payload.len())
                .is_none_or(|length| length > self.max_message_bytes)
            {
                return Err(FrameDecodeError::TooLarge);
            }
            self.message.extend_from_slice(&frame.payload);
            if frame.final_frame {
                let opcode = self
                    .message_opcode
                    .take()
                    .ok_or(FrameDecodeError::Protocol)?;
                let compressed = std::mem::replace(&mut self.compressed, false);
                return Ok(Some(FramePoll::Message {
                    opcode,
                    payload: std::mem::take(&mut self.message),
                    compressed,
                }));
            }
        }
    }

    fn next_frame(&mut self, expect_mask: bool) -> Result<Option<ParsedFrame>, FrameDecodeError> {
        if self.buffer.len() < 2 {
            return Ok(None);
        }
        let first = self.buffer[0];
        let second = self.buffer[1];
        let final_frame = first & 0x80 != 0;
        let rsv1 = first & 0x40 != 0;
        if first & 0x30 != 0 {
            return Err(FrameDecodeError::Protocol);
        }
        let opcode = first & 0x0f;
        let masked = second & 0x80 != 0;
        if masked != expect_mask {
            return Err(FrameDecodeError::Protocol);
        }
        let length_marker = second & 0x7f;
        let mut header_length = 2usize;
        let mut payload_length = u64::from(length_marker);
        if length_marker == 126 {
            if self.buffer.len() < header_length + 2 {
                return Ok(None);
            }
            payload_length = u64::from(u16::from_be_bytes([
                self.buffer[header_length],
                self.buffer[header_length + 1],
            ]));
            if payload_length < 126 {
                return Err(FrameDecodeError::Protocol);
            }
            header_length += 2;
        } else if length_marker == 127 {
            if self.buffer.len() < header_length + 8 {
                return Ok(None);
            }
            payload_length = u64::from_be_bytes(
                self.buffer[header_length..header_length + 8]
                    .try_into()
                    .map_err(|_| FrameDecodeError::Protocol)?,
            );
            if payload_length <= u16::MAX as u64 || payload_length >> 63 != 0 {
                return Err(FrameDecodeError::Protocol);
            }
            header_length += 8;
        }
        let control = opcode >= 8;
        if control && (!final_frame || payload_length > 125 || rsv1) {
            return Err(FrameDecodeError::Protocol);
        }
        let payload_length =
            usize::try_from(payload_length).map_err(|_| FrameDecodeError::TooLarge)?;
        if control && payload_length > 125 {
            return Err(FrameDecodeError::Protocol);
        }
        if !control && payload_length > self.max_message_bytes {
            return Err(FrameDecodeError::TooLarge);
        }
        let mask_length = if masked { 4 } else { 0 };
        let total_length = header_length
            .checked_add(mask_length)
            .and_then(|length| length.checked_add(payload_length))
            .ok_or(FrameDecodeError::TooLarge)?;
        if self.buffer.len() < total_length {
            return Ok(None);
        }
        let mask = if masked {
            let mask: [u8; 4] = self.buffer[header_length..header_length + 4]
                .try_into()
                .map_err(|_| FrameDecodeError::Protocol)?;
            header_length += 4;
            Some(mask)
        } else {
            None
        };
        let mut payload = self.buffer[header_length..total_length].to_vec();
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        self.buffer.drain(..total_length);
        Ok(Some(ParsedFrame {
            final_frame,
            rsv1,
            opcode,
            payload,
        }))
    }
}

/// A bounded command/event boundary around one worker-owned upstream socket.
pub struct ClientWebSocketPump {
    command_tx: SyncSender<PumpCommand>,
    event_rx: Receiver<PumpEvent>,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for ClientWebSocketPump {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientWebSocketPump")
            .field("connected", &true)
            .finish_non_exhaustive()
    }
}

impl ClientWebSocketPump {
    pub fn spawn(
        mut client: ClientWebSocket,
        config: WebSocketPumpConfig,
    ) -> Result<Self, ClientWebSocketError> {
        if config.outbound_capacity == 0
            || config.inbound_capacity == 0
            || config.outbound_capacity > MAX_PUMP_CHANNEL_CAPACITY
            || config.inbound_capacity > MAX_PUMP_CHANNEL_CAPACITY
        {
            return Err(ClientWebSocketError::new(
                500,
                "native websocket pump capacity is invalid",
            ));
        }
        client.set_max_message_bytes(config.max_message_bytes)?;
        client.set_poll_timeout(Duration::from_millis(10))?;
        let (command_tx, command_rx) = mpsc::sync_channel(config.outbound_capacity);
        let (event_tx, event_rx) = mpsc::sync_channel(config.inbound_capacity);
        let worker = std::thread::Builder::new()
            .name("emp-websocket-pump".to_owned())
            .spawn(move || pump_worker(client, command_rx, event_tx))
            .map_err(|_| ClientWebSocketError::new(500, "native websocket pump failed to start"))?;
        Ok(Self {
            command_tx,
            event_rx,
            worker: Some(worker),
        })
    }

    pub fn try_send(&self, command: PumpCommand) -> Result<(), TrySendError<PumpCommand>> {
        self.command_tx.try_send(command)
    }

    pub fn try_send_text(&self, text: String) -> Result<(), TrySendError<PumpCommand>> {
        self.try_send(PumpCommand::Text(text))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<PumpEvent, mpsc::RecvTimeoutError> {
        self.event_rx.recv_timeout(timeout)
    }

    pub fn close(&mut self) {
        self.shutdown(1000, "");
    }

    pub fn shutdown(&mut self, code: u16, reason: &str) {
        if self.worker.is_none() {
            return;
        }
        let _ = self.command_tx.send(PumpCommand::Close {
            code,
            reason: reason.to_owned(),
        });
        self.join_worker();
    }

    pub fn join(&mut self) {
        if self.worker.is_some() {
            self.close();
        }
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ClientWebSocketPump {
    fn drop(&mut self) {
        self.close();
    }
}

fn pump_worker(
    mut client: ClientWebSocket,
    command_rx: Receiver<PumpCommand>,
    event_tx: SyncSender<PumpEvent>,
) {
    let mut pending_event = None;
    loop {
        match command_rx.try_recv() {
            Ok(PumpCommand::Text(text)) => {
                if let Err(error) = client.send_text(&text) {
                    let _ = event_tx.try_send(PumpEvent::Failure {
                        status: error.status(),
                    });
                    client.close();
                    return;
                }
            }
            Ok(PumpCommand::Close { code, reason }) => {
                client.close_with(code, &reason);
                let _ = event_tx.try_send(PumpEvent::Closed { code: Some(code) });
                return;
            }
            Err(TryRecvError::Disconnected) => {
                client.close();
                return;
            }
            Err(TryRecvError::Empty) => {}
        }

        if let Some(event) = pending_event.take() {
            match event_tx.try_send(event) {
                Ok(()) => continue,
                Err(TrySendError::Full(event)) => {
                    pending_event = Some(event);
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(TrySendError::Disconnected(_)) => {
                    client.close();
                    return;
                }
            }
        }

        match client.poll_receive_text() {
            Ok(WebSocketPoll::Pending) => std::thread::sleep(Duration::from_millis(1)),
            Ok(WebSocketPoll::Text(text)) => pending_event = Some(PumpEvent::Text(text)),
            Ok(WebSocketPoll::Ping(payload)) => {
                if let Err(error) = client.send_pong(&payload) {
                    pending_event = Some(PumpEvent::Failure {
                        status: error.status(),
                    });
                }
            }
            Ok(WebSocketPoll::Closed { code }) => {
                client.close_with(code.unwrap_or(1000), "");
                let _ = event_tx.try_send(PumpEvent::Closed { code });
                return;
            }
            Err(error) => {
                client.close();
                let _ = event_tx.try_send(PumpEvent::Failure {
                    status: error.status(),
                });
                return;
            }
        }
    }
}
