//! Flat JSON with EMP's existing comma/colon spacing, written in one pass.
use serde::Serialize;
use serde_json::{Serializer, Value, ser::Formatter};
use std::io::{self, Write};

/// The same encoding feeds wire buffers and byte counters. Callers do not need
/// to inspect quoted strings or retain a second encoded copy.
pub(crate) fn write(writer: impl Write, value: &Value) -> serde_json::Result<()> {
    value.serialize(&mut Serializer::with_formatter(writer, Spaced))
}

struct Spaced;
impl Formatter for Spaced {
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.begin_array_value(writer, first)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_flat_wire_spacing_and_escaped_content() {
        let value = serde_json::json!({
            "array":[true,null,1.25,[],{}],
            "text":"雪: , \"quote\" \\ slash\n\t\u{0000}"
        });
        let mut bytes = Vec::new();
        write(&mut bytes, &value).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"array\": [true, null, 1.25, [], {}], \"text\": \"雪: , \\\"quote\\\" \\\\ slash\\n\\t\\u0000\"}"
        );
    }
}
