use super::compression::{DecompressionError, PerMessageDeflate};
use super::frame::{
    FrameDecodeError, FrameDecoder, FramePoll, WebSocketEvent, close_payload, write_frame,
};
use super::network::{HandshakeIo, Socket, read_http_head, websocket_connection};
use super::{ClientWebSocketError, WebSocketTraffic, websocket_accept};
use crate::MAX_PROXY_REQUEST_BYTES;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use url::Url;

pub struct AsyncClientWebSocket {
    pub(super) stream: Box<dyn Socket>,
    closed: bool,
    peer_close_code: Option<u16>,
    response_headers: BTreeMap<String, String>,
    pub(super) compression: Option<PerMessageDeflate>,
    pub(super) max_message_bytes: usize,
    pub(super) frame_decoder: FrameDecoder,
    traffic: WebSocketTraffic,
    write_timeout: Duration,
    protocol_close: Option<(u16, &'static str)>,
}

impl AsyncClientWebSocket {
    pub async fn connect_with_proxy(
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
        proxy: Option<&str>,
    ) -> Result<Self, ClientWebSocketError> {
        let parsed = Url::parse(url).map_err(|_| {
            ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
        })?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            return Err(ClientWebSocketError::new(
                502,
                "native upstream websocket endpoint is invalid",
            ));
        }
        let (mut stream, absolute_form, proxy_authorization) =
            websocket_connection(&parsed, proxy, timeout).await?;
        let (request, key) =
            upgrade_request(&parsed, headers, absolute_form, proxy_authorization, true)?;
        let mut handshake = HandshakeIo::new(stream.as_mut(), timeout);
        handshake
            .write_flushed(request.as_bytes())
            .await
            .map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket handshake failed")
            })?;
        let head = read_http_head(&mut handshake).await?;
        let response_headers = upgrade_response(&head, &key)?;
        let compression = response_headers
            .get("sec-websocket-extensions")
            .map(|value| PerMessageDeflate::negotiated(value))
            .transpose()?;
        Ok(Self::new(stream, response_headers, compression, timeout))
    }

    pub(super) fn new(
        stream: Box<dyn Socket>,
        response_headers: BTreeMap<String, String>,
        compression: Option<PerMessageDeflate>,
        write_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            closed: false,
            peer_close_code: None,
            response_headers,
            compression,
            max_message_bytes: MAX_PROXY_REQUEST_BYTES,
            frame_decoder: FrameDecoder::new(MAX_PROXY_REQUEST_BYTES),
            traffic: Default::default(),
            write_timeout,
            protocol_close: None,
        }
    }

    pub fn response_headers(&self) -> &BTreeMap<String, String> {
        &self.response_headers
    }
    pub const fn peer_close_code(&self) -> Option<u16> {
        self.peer_close_code
    }

    pub const fn traffic(&self) -> super::WebSocketTraffic {
        self.traffic
    }

    pub fn compression_enabled(&self) -> bool {
        self.compression.is_some()
    }

    pub fn set_max_message_bytes(&mut self, maximum: usize) -> Result<(), ClientWebSocketError> {
        if maximum == 0 || maximum > super::MAX_RESPONSES_WEBSOCKET_BYTES {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket message limit is invalid",
            ));
        }
        self.max_message_bytes = maximum;
        self.frame_decoder.set_max_message_bytes(maximum);
        Ok(())
    }

    async fn send_frame(
        &mut self,
        opcode: u8,
        payload: &[u8],
        compressed: bool,
    ) -> Result<(), ClientWebSocketError> {
        if self.closed {
            return Err(ClientWebSocketError::new(
                502,
                "native upstream websocket write failed",
            ));
        }
        let mask = client_mask()?;
        let first = 0x80 | if compressed { 0x40 } else { 0 } | opcode;
        let result = write_frame(
            &mut *self.stream,
            first,
            payload,
            Some(mask),
            Some(self.write_timeout),
        )
        .await;
        self.closed = opcode == 8 || result.is_err();
        result.map_err(client_write_error)
    }

    pub async fn send_json(&mut self, value: &Value) -> Result<(), ClientWebSocketError> {
        let payload = serde_json::to_string(value).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket request serialization failed")
        })?;
        self.send_text(&payload).await
    }

    pub async fn send_text(&mut self, text: &str) -> Result<(), ClientWebSocketError> {
        if text.len() > self.max_message_bytes {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket request is too large",
            ));
        }
        let (result, encoded_bytes) = if let Some(compression) = self.compression.as_mut() {
            let payload = compression.compress(text.as_bytes(), self.max_message_bytes)?;
            (self.send_frame(1, &payload, true).await, payload.len())
        } else {
            (self.send_frame(1, text.as_bytes(), false).await, text.len())
        };
        if result.is_ok() {
            self.traffic.decoded_sent = self
                .traffic
                .decoded_sent
                .map(|n| n.saturating_add(text.len() as u64));
            self.traffic.encoded_sent = self
                .traffic
                .encoded_sent
                .map(|n| n.saturating_add(encoded_bytes as u64));
        } else {
            self.traffic.decoded_sent = None;
            self.traffic.encoded_sent = None;
        }
        result
    }

    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<(), ClientWebSocketError> {
        if payload.len() > 125 {
            return Err(ClientWebSocketError::new(
                502,
                "invalid upstream websocket ping",
            ));
        }
        self.send_frame(10, payload, false).await
    }

    pub async fn close(&mut self, code: u16, reason: &str) {
        if !self.closed {
            let (code, reason) = self.protocol_close.take().unwrap_or((code, reason));
            let _ = self
                .send_frame(8, &close_payload(code, reason), false)
                .await;
        }
    }

    pub async fn read(&mut self) -> Result<WebSocketEvent, ClientWebSocketError> {
        if self.closed {
            return Ok(WebSocketEvent::Closed {
                code: self.peer_close_code,
                payload: Vec::new(),
            });
        }
        let poll = self
            .frame_decoder
            .read(&mut self.stream, false, self.compression.is_some())
            .await
            .map_err(|error| self.map_frame_error(error))?;
        match poll {
            FramePoll::Pending => unreachable!("async frame reads wait for I/O"),
            FramePoll::Ping(payload) => Ok(WebSocketEvent::Ping(payload)),
            FramePoll::Closed { code, payload } => {
                self.peer_close_code = code;
                Ok(WebSocketEvent::Closed { code, payload })
            }
            FramePoll::Message {
                opcode: 1,
                payload,
                compressed,
            } => {
                self.traffic.encoded_received = self
                    .traffic
                    .encoded_received
                    .map(|n| n.saturating_add(payload.len() as u64));
                let payload = if compressed {
                    match self
                        .compression
                        .as_mut()
                        .expect("compressed messages require negotiated compression")
                        .decompress(&payload, self.max_message_bytes)
                    {
                        Ok(payload) => payload,
                        Err(DecompressionError::TooLarge) => {
                            self.traffic.decoded_received = None;
                            self.protocol_close = Some((1009, "message too large"));
                            return Err(ClientWebSocketError::new(
                                502,
                                "native upstream websocket event is too large",
                            ));
                        }
                        Err(DecompressionError::Invalid) => {
                            self.traffic.decoded_received = None;
                            self.protocol_close = Some((1002, "invalid compression"));
                            return Err(ClientWebSocketError::new(
                                502,
                                "native upstream websocket compression is invalid",
                            ));
                        }
                    }
                } else {
                    payload
                };
                self.traffic.decoded_received = self
                    .traffic
                    .decoded_received
                    .map(|n| n.saturating_add(payload.len() as u64));
                match String::from_utf8(payload) {
                    Ok(text) => Ok(WebSocketEvent::Text(text)),
                    Err(_) => {
                        self.protocol_close = Some((1007, "text must be UTF-8"));
                        Err(ClientWebSocketError::new(
                            502,
                            "native upstream websocket event is not UTF-8",
                        ))
                    }
                }
            }
            FramePoll::Message { .. } => {
                self.protocol_close = Some((1003, "unsupported data frame"));
                Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket frame type is unsupported",
                ))
            }
        }
    }

    fn map_frame_error(&mut self, error: FrameDecodeError) -> ClientWebSocketError {
        self.traffic.decoded_received = None;
        self.traffic.encoded_received = None;
        match error {
            FrameDecodeError::Protocol => {
                self.protocol_close = Some((1002, "invalid frame"));
                ClientWebSocketError::new(502, "native upstream websocket frame is invalid")
            }
            FrameDecodeError::TooLarge => {
                self.protocol_close = Some((1009, "message too large"));
                ClientWebSocketError::new(502, "native upstream websocket event is too large")
            }
            FrameDecodeError::AtCapacity => {
                ClientWebSocketError::new(503, "not enough memory for websocket message")
            }
            FrameDecodeError::Io => {
                ClientWebSocketError::new(502, "native upstream websocket closed")
            }
        }
    }
}

pub(super) fn client_mask() -> Result<[u8; 4], ClientWebSocketError> {
    let mut mask = [0u8; 4];
    getrandom::getrandom(&mut mask).map_err(|_| {
        ClientWebSocketError::new(500, "native websocket randomness is unavailable")
    })?;
    Ok(mask)
}

pub(super) fn client_write_error(error: std::io::Error) -> ClientWebSocketError {
    ClientWebSocketError {
        io_kind: Some(error.kind()),
        ..ClientWebSocketError::new(502, "native upstream websocket write failed")
    }
}

pub(super) fn upgrade_request(
    parsed: &Url,
    headers: &BTreeMap<String, String>,
    absolute_form: bool,
    proxy_authorization: Option<String>,
    negotiate_compression: bool,
) -> Result<(String, String), ClientWebSocketError> {
    let host = parsed.host_str().ok_or_else(|| {
        ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
    })?;
    let port = parsed.port_or_known_default().ok_or_else(|| {
        ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
    })?;
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce).map_err(|_| {
        ClientWebSocketError::new(500, "native websocket randomness is unavailable")
    })?;
    let key = STANDARD.encode(nonce);
    let authority = if parsed.port().is_some() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    };
    let mut path = parsed.path().to_owned();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    if absolute_form {
        path = format!("{}://{authority}{path}", parsed.scheme());
    }
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
    );
    if negotiate_compression {
        request
            .push_str("Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n");
    }
    if let Some(authorization) = proxy_authorization {
        request.push_str("Proxy-Authorization: ");
        request.push_str(&authorization);
        request.push_str("\r\n");
    }
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "upgrade"
                | "connection"
                | "sec-websocket-version"
                | "sec-websocket-key"
                | "sec-websocket-extensions"
        ) {
            continue;
        }
        if name
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == b':')
            || value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
        {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket header is invalid",
            ));
        }
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    Ok((request, key))
}

pub(super) fn upgrade_response(
    head: &[u8],
    key: &str,
) -> Result<BTreeMap<String, String>, ClientWebSocketError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| ClientWebSocketError::new(502, "native websocket handshake is invalid"))?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| ClientWebSocketError::new(502, "native websocket handshake is invalid"))?;
    if status != 101 {
        return Err(ClientWebSocketError::upgrade_rejected(
            status,
            "native upstream websocket upgrade was rejected",
        ));
    }
    let response_headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if response_headers
        .get("upgrade")
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
        || response_headers.get("connection").is_none_or(|value| {
            !value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
        || response_headers.get("sec-websocket-accept")
            != Some(
                &websocket_accept(key).map_err(|_| {
                    ClientWebSocketError::new(500, "native websocket accept failed")
                })?,
            )
    {
        return Err(ClientWebSocketError::new(
            502,
            "native websocket handshake is invalid",
        ));
    }
    Ok(response_headers)
}
