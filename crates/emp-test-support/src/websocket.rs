mod client;
mod downstream;

pub use client::ClientWebSocket;
pub use downstream::WebSocketConnection;
use std::fmt;

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
