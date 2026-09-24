//! Upstream WebSocket client lifecycle and frame I/O.

use super::*;

impl ClientWebSocket {
    pub fn connect(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        Self::connect_with_proxy(url, headers, timeout, None)
    }

    pub fn connect_with_proxy(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
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
        let (stream, absolute_form, proxy_authorization) =
            websocket_connection(&parsed, proxy, timeout)?;
        Self::handshake(
            stream,
            &parsed,
            headers,
            absolute_form,
            proxy_authorization,
            true,
            Duration::from_secs(300),
        )
    }

    /// Read-only Codex control transport. It never consults proxy settings.
    pub fn connect_local(
        path: &std::path::Path,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        std::fs::metadata(path).map_err(local_socket_error)?;
        let address = socket2::SockAddr::unix(path).map_err(local_socket_error)?;
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(local_socket_error)?;
        socket
            .connect_timeout(&address, timeout)
            .map_err(local_socket_error)?;
        socket
            .set_read_timeout(Some(timeout))
            .map_err(local_socket_error)?;
        socket
            .set_write_timeout(Some(timeout))
            .map_err(local_socket_error)?;
        let url = Url::parse("ws://localhost/").expect("constant local WebSocket URL");
        let mut client = Self::handshake(
            Box::new(socket),
            &url,
            &std::collections::BTreeMap::new(),
            false,
            None,
            false,
            timeout,
        )?;
        client.set_max_message_bytes(2 * 1024 * 1024)?;
        client.local_control = true;
        Ok(client)
    }

    fn handshake(
        mut stream: Box<dyn ReadWrite>,
        parsed: &Url,
        headers: &std::collections::BTreeMap<String, String>,
        absolute_form: bool,
        proxy_authorization: Option<String>,
        negotiate_compression: bool,
        read_timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
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
            request.push_str(
                "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n",
            );
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
        stream
            .write_all(request.as_bytes())
            .and_then(|_| stream.flush())
            .map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket handshake failed")
            })?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= 64 * 1024 {
                return Err(ClientWebSocketError::new(
                    502,
                    "native websocket handshake is too large",
                ));
            }
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket handshake failed")
            })?;
            head.push(byte[0]);
        }
        let text = std::str::from_utf8(&head)
            .map_err(|_| ClientWebSocketError::new(502, "native websocket handshake is invalid"))?;
        let mut lines = text.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| {
                ClientWebSocketError::new(502, "native websocket handshake is invalid")
            })?;
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
                != Some(&websocket_accept(&key).map_err(|_| {
                    ClientWebSocketError::new(500, "native websocket accept failed")
                })?)
        {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket handshake is invalid",
            ));
        }
        let compression = match response_headers.get("sec-websocket-extensions") {
            Some(_) if !negotiate_compression => {
                return Err(ClientWebSocketError::new(
                    502,
                    "local control websocket returned an unsolicited extension",
                ));
            }
            Some(value) => Some(PerMessageDeflate::negotiated(value)?),
            None => None,
        };
        stream.set_read_timeout(Some(read_timeout)).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket timeout setup failed")
        })?;
        Ok(Self {
            stream,
            closed: false,
            peer_close_code: None,
            response_headers,
            compression,
            max_message_bytes: MAX_PROXY_REQUEST_BYTES,
            frame_decoder: FrameDecoder::new(MAX_PROXY_REQUEST_BYTES),
            local_control: false,
        })
    }
    pub fn response_headers(&self) -> &std::collections::BTreeMap<String, String> {
        &self.response_headers
    }
    pub const fn peer_close_code(&self) -> Option<u16> {
        self.peer_close_code
    }

    pub fn set_max_message_bytes(&mut self, maximum: usize) -> Result<(), ClientWebSocketError> {
        if maximum == 0 || maximum > MAX_PROXY_REQUEST_BYTES {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket message limit is invalid",
            ));
        }
        self.max_message_bytes = maximum;
        self.frame_decoder.set_max_message_bytes(maximum);
        Ok(())
    }

    pub(crate) fn set_poll_timeout(&self, timeout: Duration) -> Result<(), ClientWebSocketError> {
        self.stream.set_read_timeout(Some(timeout)).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket timeout setup failed")
        })
    }
    fn send_frame(
        &mut self,
        opcode: u8,
        payload: &[u8],
        compressed: bool,
    ) -> Result<(), ClientWebSocketError> {
        let mut mask = [0u8; 4];
        getrandom::getrandom(&mut mask).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket randomness is unavailable")
        })?;
        let mut frame = vec![0x80 | if compressed { 0x40 } else { 0 } | opcode];
        match payload.len() {
            length if length < 126 => frame.push(0x80 | length as u8),
            length if length <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.stream
            .write_all(&frame)
            .and_then(|_| self.stream.flush())
            .map_err(|_| ClientWebSocketError::new(502, "native upstream websocket write failed"))
    }
    pub fn send_json(&mut self, value: &Value) -> Result<(), ClientWebSocketError> {
        let payload = serde_json::to_string(value).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket request serialization failed")
        })?;
        self.send_text(&payload)
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), ClientWebSocketError> {
        if text.len() > self.max_message_bytes {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket request is too large",
            ));
        }
        if let Some(compression) = self.compression.as_mut() {
            let payload = compression.compress(text.as_bytes())?;
            self.send_frame(1, &payload, true)
        } else {
            self.send_frame(1, text.as_bytes(), false)
        }
    }

    pub fn send_pong(&mut self, payload: &[u8]) -> Result<(), ClientWebSocketError> {
        if payload.len() > 125 {
            return Err(ClientWebSocketError::new(
                502,
                "invalid upstream websocket ping",
            ));
        }
        self.send_frame(10, payload, false)
    }

    pub fn close_with(&mut self, code: u16, reason: &str) {
        if self.closed {
            return;
        }
        let bytes = reason.as_bytes();
        let mut count = bytes.len().min(123);
        while !reason.is_char_boundary(count) {
            count -= 1;
        }
        let mut payload = Vec::with_capacity(count + 2);
        payload.extend_from_slice(&code.to_be_bytes());
        payload.extend_from_slice(&bytes[..count]);
        let _ = self.send_frame(8, &payload, false);
        self.closed = true;
    }

    pub fn poll_receive_text(&mut self) -> Result<WebSocketPoll<String>, ClientWebSocketError> {
        if self.closed {
            return Ok(WebSocketPoll::Closed {
                code: self.peer_close_code,
            });
        }
        let poll = self
            .frame_decoder
            .poll(&mut *self.stream, false, self.compression.is_some())
            .map_err(|error| self.map_frame_error(error))?;
        match poll {
            FramePoll::Pending => Ok(WebSocketPoll::Pending),
            FramePoll::Ping(payload) => Ok(WebSocketPoll::Ping(payload)),
            FramePoll::Closed { code, .. } => {
                self.peer_close_code = code;
                Ok(WebSocketPoll::Closed { code })
            }
            FramePoll::Message {
                opcode: 1,
                payload,
                compressed,
            } => {
                let payload = if compressed {
                    match self
                        .compression
                        .as_mut()
                        .expect("compressed messages require negotiated compression")
                        .decompress(&payload, self.max_message_bytes)
                    {
                        Ok(payload) => payload,
                        Err(DecompressionError::TooLarge) => {
                            self.close_with(1009, "message too large");
                            return Err(ClientWebSocketError::new(
                                502,
                                "native upstream websocket event is too large",
                            ));
                        }
                        Err(DecompressionError::Invalid) => {
                            self.close_with(1002, "invalid compression");
                            return Err(ClientWebSocketError::new(
                                502,
                                "native upstream websocket compression is invalid",
                            ));
                        }
                    }
                } else {
                    payload
                };
                if payload.len() > self.max_message_bytes {
                    self.close_with(1009, "message too large");
                    return Err(ClientWebSocketError::new(
                        502,
                        "native upstream websocket event is too large",
                    ));
                }
                match String::from_utf8(payload) {
                    Ok(text) => Ok(WebSocketPoll::Text(text)),
                    Err(_) => {
                        self.close_with(1007, "text must be UTF-8");
                        Err(ClientWebSocketError::new(
                            502,
                            "native upstream websocket event is not UTF-8",
                        ))
                    }
                }
            }
            FramePoll::Message { .. } => {
                self.close_with(1003, "unsupported data frame");
                Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket frame type is unsupported",
                ))
            }
        }
    }

    fn map_frame_error(&mut self, error: FrameDecodeError) -> ClientWebSocketError {
        match error {
            FrameDecodeError::Protocol => {
                self.close_with(1002, "invalid frame");
                ClientWebSocketError::new(502, "native upstream websocket frame is invalid")
            }
            FrameDecodeError::TooLarge => {
                self.close_with(1009, "message too large");
                ClientWebSocketError::new(502, "native upstream websocket event is too large")
            }
            FrameDecodeError::Io => ClientWebSocketError::new(
                if self.local_control { 503 } else { 502 },
                "native upstream websocket closed",
            ),
        }
    }
    pub fn receive_json(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        loop {
            match self.receive_value()? {
                Some(value) if !value.is_object() => continue,
                value => return Ok(value),
            }
        }
    }

    /// Control RPC skips unrelated JSON values; native Responses requires objects.
    pub fn receive_value(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        loop {
            match self.poll_receive_text()? {
                WebSocketPoll::Pending => std::thread::sleep(Duration::from_millis(1)),
                WebSocketPoll::Ping(payload) => self.send_pong(&payload)?,
                WebSocketPoll::Closed { code } => {
                    self.close_with(code.unwrap_or(1000), "");
                    return Ok(None);
                }
                WebSocketPoll::Text(text) => match serde_json::from_str(&text) {
                    Ok(value) => return Ok(Some(value)),
                    Err(_) if !self.local_control => continue,
                    Err(_) => {
                        return Err(ClientWebSocketError::new(
                            502,
                            "local control websocket event is invalid JSON",
                        ));
                    }
                },
            }
        }
    }
    pub fn close(&mut self) {
        self.close_with(1000, "");
    }
}
impl Drop for ClientWebSocket {
    fn drop(&mut self) {
        self.close();
    }
}
