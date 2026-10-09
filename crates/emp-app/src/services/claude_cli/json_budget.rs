//! Count encoded JSON without allocating another copy of the value.

use serde::Serialize;
use std::io::{self, Write};

/// Account for blocks retained together in a Claude CLI input. Unlike streamed
/// output, these blocks stay in memory until the complete input is encoded.
#[derive(Debug)]
pub(super) struct JsonByteBudget {
    remaining: usize,
}

impl JsonByteBudget {
    pub(super) fn new(bytes: usize) -> Self {
        Self { remaining: bytes }
    }

    pub(super) fn charge<T: Serialize + ?Sized>(&mut self, value: &T) -> serde_json::Result<()> {
        serde_json::to_writer(self, value)
    }

    pub(super) fn reserve(&mut self, bytes: usize) -> io::Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| io::Error::other("JSON byte budget exceeded"))?;
        Ok(())
    }
}

impl Write for JsonByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.reserve(bytes.len())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
