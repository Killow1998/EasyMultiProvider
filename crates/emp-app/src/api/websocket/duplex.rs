//! Read either side of a native turn without a polling worker or event queue.
use emp_transport::{ClientWebSocket, WebSocketConnection, WebSocketPoll};
use polling::{Event, Events, Poller};
use serde_json::Value;
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) enum Message {
    Upstream(Value),
    Downstream(String),
    DownstreamClosed,
}

struct Probe {
    socket: TcpStream,
    poller: Arc<Poller>,
    key: usize,
    timeout: Option<Duration>,
    ready: bool,
}

impl Probe {
    fn new(socket: TcpStream, poller: Arc<Poller>, key: usize) -> io::Result<Self> {
        let timeout = socket.read_timeout()?;
        // SAFETY: the owned socket stays alive until Drop unregisters it.
        unsafe { poller.add(&socket, Event::readable(key))? };
        let probe = Self {
            socket,
            poller,
            key,
            timeout,
            ready: true,
        };
        // Bound spurious readiness and reads after draining TLS/frame buffers.
        // Idle waits themselves use the OS poller, with no periodic wakeup.
        probe
            .socket
            .set_read_timeout(Some(Duration::from_millis(1)))?;
        Ok(probe)
    }

    fn pending(&mut self) -> io::Result<()> {
        self.ready = false;
        self.poller.modify(&self.socket, Event::readable(self.key))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.poller.delete(&self.socket);
        let _ = self.socket.set_read_timeout(self.timeout);
    }
}

pub(super) struct Duplex {
    upstream: Probe,
    downstream: Probe,
    events: Events,
    idle: Option<Duration>,
    last_upstream: Instant,
}

impl Duplex {
    pub(super) fn new(client: &ClientWebSocket, downstream: &TcpStream) -> io::Result<Self> {
        let poller = Arc::new(Poller::new()?);
        let upstream = Probe::new(client.readiness_stream()?, Arc::clone(&poller), 0)?;
        let idle = upstream.timeout;
        let downstream = Probe::new(downstream.try_clone()?, poller, 1)?;
        Ok(Self {
            upstream,
            downstream,
            idle,
            events: Events::with_capacity(std::num::NonZeroUsize::new(2).unwrap()),
            last_upstream: Instant::now(),
        })
    }

    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.events.clear();
        match self.upstream.poller.wait(&mut self.events, timeout) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            result => {
                result?;
            }
        }
        for event in self
            .events
            .iter()
            .filter(|event| event.readable || event.writable)
        {
            match event.key {
                0 => self.upstream.ready = true,
                1 => self.downstream.ready = true,
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn next(
        &mut self,
        client: &mut ClientWebSocket,
        downstream: &mut WebSocketConnection<'_, TcpStream>,
        read_downstream: bool,
    ) -> io::Result<Option<Message>> {
        loop {
            // A stream of buffered upstream messages must not starve an interrupt.
            self.wait(Some(Duration::ZERO))?;
            let remaining = self
                .idle
                .map(|idle| idle.saturating_sub(self.last_upstream.elapsed()));
            if remaining == Some(Duration::ZERO) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            if read_downstream && self.downstream.ready {
                match downstream.poll_text() {
                    Ok(WebSocketPoll::Text(text)) => return Ok(Some(Message::Downstream(text))),
                    Ok(WebSocketPoll::Ping(payload)) => {
                        downstream.send_pong(&payload).map_err(io::Error::other)?;
                        continue;
                    }
                    Ok(WebSocketPoll::Pending) => self.downstream.pending()?,
                    Ok(WebSocketPoll::Closed { .. }) | Err(_) => {
                        return Ok(Some(Message::DownstreamClosed));
                    }
                }
            }
            if self.upstream.ready {
                match client.poll_receive_text().map_err(io::Error::other)? {
                    WebSocketPoll::Text(text) => {
                        self.last_upstream = Instant::now();
                        // Match Codex: skip malformed or unrelated JSON events.
                        if let Ok(value @ Value::Object(_)) = serde_json::from_str(&text) {
                            return Ok(Some(Message::Upstream(value)));
                        }
                        continue;
                    }
                    WebSocketPoll::Ping(payload) => {
                        self.last_upstream = Instant::now();
                        client.send_pong(&payload).map_err(io::Error::other)?;
                        continue;
                    }
                    WebSocketPoll::Pending => self.upstream.pending()?,
                    WebSocketPoll::Closed { .. } => return Ok(None),
                }
            }
            self.wait(remaining)?;
        }
    }
}
