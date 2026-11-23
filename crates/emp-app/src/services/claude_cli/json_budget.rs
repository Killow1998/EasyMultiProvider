//! Count encoded JSON without allocating another copy of the value.

use serde::Serialize;
use std::io::{self, Write};

/// Budget all CLI input blocks together: they remain in memory until encoding completes.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_bytes_include_escaping_and_accumulate_across_blocks() {
        let mut budget = JsonByteBudget::new(8);
        budget.charge("\"").unwrap(); // Four JSON bytes, including the quotes.
        budget.charge("\"").unwrap();
        assert!(budget.charge("").is_err());
    }
}
