use super::network::io_timeout;
use crate::{RequestBudget, RequestLimits, TransportKind};
use std::{fmt, io, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, PartialEq, Eq)]
pub enum WebSocketEvent {
    Text(String),
    Ping(Vec<u8>),
    Closed { code: Option<u16>, payload: Vec<u8> },
}

impl fmt::Debug for WebSocketEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Ping(_) => formatter.write_str("Ping(<redacted>)"),
            Self::Closed { code, .. } => formatter
                .debug_struct("Closed")
                .field("code", code)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecodeError {
    Protocol,
    TooLarge,
    AtCapacity,
    Io,
}

pub enum FramePoll {
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

pub struct FrameDecoder {
    buffer: Vec<u8>,
    needed: usize,
    message: Vec<u8>,
    message_opcode: Option<u8>,
    compressed: bool,
    max_message_bytes: usize,
    request_limits: Option<std::sync::Arc<RequestLimits>>,
    request_budget: Option<RequestBudget>,
}

impl FrameDecoder {
    pub fn new(max_message_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            needed: 2,
            message: Vec::new(),
            message_opcode: None,
            compressed: false,
            max_message_bytes,
            request_limits: None,
            request_budget: None,
        }
    }

    pub fn set_max_message_bytes(&mut self, maximum: usize) {
        self.max_message_bytes = maximum;
    }

    pub(crate) fn limit_request_memory(&mut self, limits: std::sync::Arc<RequestLimits>) {
        self.request_limits = Some(limits);
    }

    pub(crate) fn take_request_budget(&mut self) -> Option<RequestBudget> {
        self.request_budget.take()
    }

    pub fn feed(&mut self, prefix: &[u8]) -> Result<(), FrameDecodeError> {
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

    pub fn poll<R: std::io::Read + ?Sized>(
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

    pub(super) async fn read(
        &mut self,
        stream: &mut (impl AsyncRead + Unpin),
        expect_mask: bool,
        allow_compression: bool,
    ) -> Result<FramePoll, FrameDecodeError> {
        loop {
            if let Some(event) = self.parse_buffered(expect_mask, allow_compression)? {
                return Ok(event);
            }
            let mut chunk = [0u8; 8192];
            let needed = (self.needed - self.buffer.len()).min(chunk.len());
            match stream.read(&mut chunk[..needed]).await {
                Ok(0) => {
                    return Ok(FramePoll::Closed {
                        code: None,
                        payload: Vec::new(),
                    });
                }
                Ok(count) => self.buffer.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
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
            let opcode = match frame.opcode {
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
                10 => continue,
                1 | 2 => {
                    if self.message_opcode.is_some() {
                        return Err(FrameDecodeError::Protocol);
                    }
                    if frame.rsv1 && !allow_compression {
                        return Err(FrameDecodeError::Protocol);
                    }
                    self.message_opcode = Some(frame.opcode);
                    self.compressed = frame.rsv1;
                    frame.opcode
                }
                0 if !frame.rsv1 => self.message_opcode.ok_or(FrameDecodeError::Protocol)?,
                _ => return Err(FrameDecodeError::Protocol),
            };
            if self.message.is_empty() {
                self.message = frame.payload;
            } else {
                self.message.extend_from_slice(&frame.payload);
            }
            if frame.final_frame {
                self.message_opcode = None;
                let compressed = std::mem::take(&mut self.compressed);
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
            self.needed = 2;
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
                self.needed = header_length + 2;
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
                self.needed = header_length + 8;
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
        if !control {
            let size = self
                .message
                .len()
                .checked_add(payload_length)
                .ok_or(FrameDecodeError::TooLarge)?;
            if size > self.max_message_bytes {
                return Err(FrameDecodeError::TooLarge);
            }
            if let Some(limits) = &self.request_limits
                && size > crate::MAX_PROXY_REQUEST_BYTES
            {
                self.request_budget
                    .get_or_insert_with(|| limits.request(TransportKind::WebSocket))
                    .ensure(size)
                    .map_err(|_| FrameDecodeError::AtCapacity)?;
            }
        }
        let mask_length = if masked { 4 } else { 0 };
        let total_length = header_length
            .checked_add(mask_length)
            .and_then(|length| length.checked_add(payload_length))
            .ok_or(FrameDecodeError::TooLarge)?;
        if self.buffer.len() < total_length {
            self.needed = total_length;
            if payload_length > crate::MAX_PROXY_REQUEST_BYTES
                && self.buffer.capacity() < total_length
            {
                self.buffer
                    .try_reserve_exact(total_length - self.buffer.len())
                    .map_err(|_| FrameDecodeError::AtCapacity)?;
            }
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
        let mut payload = if self.buffer.len() - total_length <= payload_length {
            let remainder = self.buffer.split_off(total_length);
            let mut payload = std::mem::replace(&mut self.buffer, remainder);
            payload.drain(..header_length);
            payload
        } else {
            let payload = self.buffer[header_length..total_length].to_vec();
            self.buffer.drain(..total_length);
            payload
        };
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok(Some(ParsedFrame {
            final_frame,
            rsv1,
            opcode,
            payload,
        }))
    }
}

pub(super) async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin + ?Sized),
    first: u8,
    payload: &[u8],
    mask: Option<[u8; 4]>,
    timeout: Option<Duration>,
) -> io::Result<()> {
    let mut frame = Vec::with_capacity(14);
    frame.push(first);
    frame.extend(super::frame_length_prefix(payload.len()));
    if let Some(mask) = mask {
        frame[1] |= 0x80;
        frame.extend_from_slice(&mask);
        const MASK_ALIGNED_WRITE_CHUNK_BYTES: usize = 64 * 1024;
        for chunk in payload.chunks(MASK_ALIGNED_WRITE_CHUNK_BYTES) {
            frame.extend(
                chunk
                    .iter()
                    .enumerate()
                    .map(|(index, byte)| byte ^ mask[index % 4]),
            );
            write_bytes(stream, &frame, timeout).await?;
            frame.clear();
        }
        if !frame.is_empty() {
            write_bytes(stream, &frame, timeout).await?;
        }
    } else {
        write_bytes(stream, &frame, timeout).await?;
        write_bytes(stream, payload, timeout).await?;
    }
    if let Some(timeout) = timeout {
        io_timeout(timeout, stream.flush()).await
    } else {
        stream.flush().await
    }
}

async fn write_bytes(
    stream: &mut (impl AsyncWrite + Unpin + ?Sized),
    mut bytes: &[u8],
    timeout: Option<Duration>,
) -> io::Result<()> {
    let Some(timeout) = timeout else {
        return stream.write_all(bytes).await;
    };
    while !bytes.is_empty() {
        match io_timeout(timeout, stream.write(bytes)).await {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut count = reason.len().min(123);
    while !reason.is_char_boundary(count) {
        count -= 1;
    }
    let mut payload = Vec::with_capacity(count + 2);
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(&reason.as_bytes()[..count]);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{self, Read};

    #[tokio::test]
    async fn trickled_frame_yields_at_deadline_and_resumes_without_losing_bytes() {
        use emp_test_support::{Deadline, test_timeout};
        use std::net::{TcpListener, TcpStream};
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut peer = tokio::net::TcpStream::from_std(peer).unwrap();
        let payload = b"abcdefghijklmnopqrstuvwxyz";
        peer.write_all(&[0x81, payload.len() as u8]).await.unwrap();

        let (started, reading) = tokio::sync::oneshot::channel();
        let mut decoder = tokio::task::spawn_blocking(move || {
            let mut decoder = FrameDecoder::new(1024);
            let deadline = Deadline::after(test_timeout(Duration::from_millis(200)));
            let mut input = deadline.io(client);
            started.send(()).unwrap();
            let poll = decoder.poll(&mut input, false, false).unwrap();
            (decoder, input, poll)
        });
        reading.await.unwrap();
        let mut sent = 0;
        let (mut decoder, mut input, poll) = loop {
            tokio::select! {
                result = &mut decoder => break result.unwrap(),
                _ = tokio::time::sleep(test_timeout(Duration::from_millis(25)).duration()), if sent < payload.len() => {
                    peer.write_all(&payload[sent..=sent]).await.unwrap();
                    sent += 1;
                }
            }
        };
        assert!(matches!(poll, FramePoll::Pending));
        assert!(sent < payload.len());
        peer.write_all(&payload[sent..]).await.unwrap();
        let decoded = tokio::task::spawn_blocking(move || {
            input.set_deadline(&Deadline::after(test_timeout(Duration::from_secs(5))));
            decoder.poll(&mut input, false, false).unwrap()
        })
        .await
        .unwrap();
        assert!(
            matches!(decoded, FramePoll::Message { opcode: 1, payload: decoded, compressed: false } if decoded == payload)
        );
    }

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

    #[tokio::test]
    async fn rejected_header_does_not_reserve_payload_storage() {
        use crate::{MAX_RESPONSES_WEBSOCKET_BYTES, MemoryStatus, RequestLimitsConfig};

        let maximum = MAX_RESPONSES_WEBSOCKET_BYTES;
        let limits = RequestLimits::new(
            RequestLimitsConfig::default(),
            || Some(MemoryStatus::available(2 * 1024 * 1024 * 1024)),
            || 0,
            "frame-budget",
        )
        .unwrap();
        let mut competing_request = limits.request(TransportKind::WebSocket);
        competing_request.ensure(maximum).unwrap();

        for (size, expected) in [
            (maximum, FrameDecodeError::AtCapacity),
            (maximum + 1, FrameDecodeError::TooLarge),
        ] {
            let mut header = [0; 10];
            header[..2].copy_from_slice(&[0x81, 0xff]);
            header[2..].copy_from_slice(&(size as u64).to_be_bytes());
            let mut decoder = FrameDecoder::new(maximum);
            decoder.limit_request_memory(limits.clone());
            assert!(matches!(
                decoder.read(&mut &header[..], true, false).await,
                Err(error) if error == expected
            ));
            assert!(
                decoder.buffer.capacity() <= 16,
                "only the header may be allocated"
            );
        }
    }

    #[test]
    fn websocket_poll_debug_redacts_payloads() {
        use emp_test_support::websocket::WebSocketPoll;

        assert!(!format!("{:?}", WebSocketPoll::Text("private".to_owned())).contains("private"));
        assert!(!format!("{:?}", WebSocketEvent::Text("private".to_owned())).contains("private"));
    }
}
