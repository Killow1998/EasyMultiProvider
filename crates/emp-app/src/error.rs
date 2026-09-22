//! Error.

use emp_state::ConfigError;
use emp_state::FilesystemError;
use emp_state::WebSessionError;
use emp_transport::RequestLimitsError;

#[derive(Debug)]
pub(crate) enum AppError {
    HostNotLoopback,
    Io(std::io::Error),
    ServerStopped,
    ServiceOwned,
    RandomUnavailable,
    WebSession(WebSessionError),
    Config(ConfigError),
    Filesystem(FilesystemError),
    Transport(emp_transport::HttpTransportError),
    RequestLimits(RequestLimitsError),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostNotLoopback => {
                formatter.write_str("host must be 127.0.0.1 for local-only management")
            }
            Self::Io(error) => write!(formatter, "{error}"),
            Self::ServiceOwned => {
                formatter.write_str("another EMP service owns this configuration")
            }
            Self::ServerStopped => formatter.write_str("server task stopped before shutdown"),
            Self::RandomUnavailable => formatter.write_str("secure randomness is unavailable"),
            Self::WebSession(error) => write!(formatter, "{error}"),
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Filesystem(error) => write!(formatter, "{error}"),
            Self::Transport(error) => write!(formatter, "{error}"),
            Self::RequestLimits(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<WebSessionError> for AppError {
    fn from(value: WebSessionError) -> Self {
        Self::WebSession(value)
    }
}

impl From<ConfigError> for AppError {
    fn from(value: ConfigError) -> Self {
        Self::Config(value)
    }
}

impl From<FilesystemError> for AppError {
    fn from(value: FilesystemError) -> Self {
        Self::Filesystem(value)
    }
}

impl From<emp_transport::HttpTransportError> for AppError {
    fn from(value: emp_transport::HttpTransportError) -> Self {
        Self::Transport(value)
    }
}

impl From<RequestLimitsError> for AppError {
    fn from(value: RequestLimitsError) -> Self {
        Self::RequestLimits(value)
    }
}
