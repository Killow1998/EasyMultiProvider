use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Read},
};

pub fn parse_head(head: &str) -> (String, impl Iterator<Item = (String, String)> + '_) {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    (first, headers)
}

/// Reads the HTTP message head and body. Without a length or chunked framing,
/// reads the body to EOF when `until_eof` is true.
pub fn read_message(
    stream: &mut impl Read,
    until_eof: bool,
) -> io::Result<(String, BTreeMap<String, String>, Vec<u8>)> {
    let mut input = BufReader::new(stream);
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if input.read_until(b'\n', &mut head)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete HTTP head",
            ));
        }
    }
    let head = std::str::from_utf8(&head)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (first, fields) = parse_head(head);
    let mut headers = BTreeMap::new();
    let mut length = None;
    for (name, value) in fields {
        if name == "content-length" {
            let declared = value
                .parse::<usize>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if let Some(previous) = length.replace(declared)
                && previous != declared
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "conflicting Content-Length fields",
                ));
            }
        }
        headers.insert(name, value);
    }
    if length.is_some() && headers.contains_key("transfer-encoding") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Content-Length and Transfer-Encoding cannot be combined",
        ));
    }
    let mut body = Vec::new();
    if let Some(length) = length {
        input.take(length as u64).read_to_end(&mut body)?;
        if body.len() != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP body ended before Content-Length bytes",
            ));
        }
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        Body::new(input, true).read_to_end(&mut body)?;
    } else if until_eof {
        input.read_to_end(&mut body)?;
    }
    Ok((first, headers, body))
}

pub struct Body<R> {
    input: R,
    chunked: Option<ChunkDecoder>,
}

#[derive(Default)]
pub struct ChunkDecoder {
    state: ChunkState,
    line: Vec<u8>,
}

#[derive(Default)]
enum ChunkState {
    #[default]
    Header,
    Data(usize),
    Ending(usize),
    Trailers,
    End,
}

impl ChunkDecoder {
    pub fn decode<'a>(&mut self, input: &mut &'a [u8]) -> io::Result<&'a [u8]> {
        while !input.is_empty() {
            match self.state {
                ChunkState::Header | ChunkState::Trailers => {
                    let Some(newline) = input.iter().position(|&byte| byte == b'\n') else {
                        self.line.extend_from_slice(input);
                        *input = &[];
                        break;
                    };
                    self.line.extend_from_slice(&input[..=newline]);
                    *input = &input[newline + 1..];
                    if matches!(self.state, ChunkState::Header) {
                        let Ok(httparse::Status::Complete((_, length))) =
                            httparse::parse_chunk_size(&self.line)
                        else {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid HTTP chunk size line",
                            ));
                        };
                        let length = usize::try_from(length)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        self.state = if length == 0 {
                            ChunkState::Trailers
                        } else {
                            ChunkState::Data(length)
                        };
                    } else {
                        let line = self.line.strip_suffix(b"\r\n").ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid HTTP chunk line ending",
                            )
                        })?;
                        if trailer_is_end(line)? {
                            self.state = ChunkState::End;
                        }
                    }
                    self.line.clear();
                }
                ChunkState::Data(remaining) => {
                    let count = remaining.min(input.len());
                    let (body, rest) = input.split_at(count);
                    *input = rest;
                    self.state = if count == remaining {
                        ChunkState::Ending(0)
                    } else {
                        ChunkState::Data(remaining - count)
                    };
                    return Ok(body);
                }
                ChunkState::Ending(matched) => {
                    let expected = &b"\r\n"[matched..];
                    let count = expected.len().min(input.len());
                    if input[..count] != expected[..count] {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid HTTP chunk ending",
                        ));
                    }
                    *input = &input[count..];
                    self.state = if count == expected.len() {
                        ChunkState::Header
                    } else {
                        ChunkState::Ending(matched + count)
                    };
                }
                ChunkState::End => break,
            }
        }
        Ok(&[])
    }

    pub fn at_chunk_ending(&self) -> bool {
        matches!(self.state, ChunkState::Ending(0))
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, ChunkState::End)
    }

    pub fn finish(&self) -> io::Result<()> {
        let message = match self.state {
            ChunkState::Header => "incomplete HTTP chunk header",
            ChunkState::Data(_) => "incomplete HTTP chunk data",
            ChunkState::Ending(_) => "incomplete HTTP chunk ending",
            ChunkState::Trailers => "incomplete HTTP chunk trailer",
            ChunkState::End => return Ok(()),
        };
        Err(io::Error::new(io::ErrorKind::UnexpectedEof, message))
    }
}

impl<R> Body<R> {
    pub fn new(input: R, chunked: bool) -> Self {
        Self {
            input,
            chunked: chunked.then(ChunkDecoder::default),
        }
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.input
    }
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let Some(decoder) = &mut self.chunked else {
            return self.input.read(buffer);
        };
        while !decoder.is_finished() {
            let available = self.input.fill_buf()?;
            let length = available.len().min(buffer.len());
            if length == 0 {
                decoder.finish()?;
                break;
            }
            let mut input = &available[..length];
            let body = decoder.decode(&mut input)?;
            let count = body.len();
            buffer[..count].copy_from_slice(body);
            let consumed = length - input.len();
            self.input.consume(consumed);
            if count != 0 {
                return Ok(count);
            }
        }
        Ok(0)
    }
}

fn trailer_is_end(line: &[u8]) -> io::Result<bool> {
    if line.is_empty() {
        return Ok(true);
    }
    let valid = line
        .iter()
        .position(|&byte| byte == b':')
        .is_some_and(|colon| {
            colon > 0
                && line[..colon]
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^`_|~".contains(byte))
                && line[colon + 1..]
                    .iter()
                    .all(|&byte| byte == b'\t' || (byte >= b' ' && byte != 127))
        });
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid chunk trailer field",
        ));
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse;

    #[test]
    fn close_delimited_message_reads_the_tail_after_a_terminal_frame() {
        let first = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                      data: {\"type\":\"response.completed\"}\r\n\r\n";
        let mut response = first.as_slice().chain(b"data: {".as_slice());
        let (_, _, body) = read_message(&mut response, true).unwrap();

        let mut frames = sse::frames(&body);
        assert_eq!(
            frames.next().unwrap().unwrap().data().as_deref(),
            Some(r#"{"type":"response.completed"}"#),
        );
        assert_eq!(
            frames
                .next()
                .expect("read the late SSE tail")
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof,
        );
    }

    #[test]
    fn chunk_body_delivers_complete_frame_before_reporting_a_bad_chunk_end() {
        let payload = "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
        for (ending, kind, message) in [
            (
                "",
                io::ErrorKind::UnexpectedEof,
                "incomplete HTTP chunk ending",
            ),
            (
                "XX",
                io::ErrorKind::InvalidData,
                "invalid HTTP chunk ending",
            ),
        ] {
            let wire = format!("{:x}\r\n{payload}{ending}", payload.len());
            let mut body = BufReader::new(Body::new(wire.as_bytes(), true));
            let frame = sse::read_frame(&mut body).unwrap().unwrap();
            assert_eq!(frame.raw(), payload);
            let error = sse::read_frame(&mut body).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn chunk_decoder_delivers_a_frame_spanning_two_chunks() {
        let mut decoder = ChunkDecoder::default();
        let mut first = b"b\r\ndata: first".as_slice();
        let mut body = decoder.decode(&mut first).unwrap().to_vec();
        assert_eq!(
            sse::read_frame(&mut body.as_slice()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
        );

        let mut second = b"\r\n2\r\n\n\n".as_slice();
        body.extend_from_slice(decoder.decode(&mut second).unwrap());
        let frame = sse::read_frame(&mut body.as_slice()).unwrap().unwrap();
        assert_eq!(frame.raw(), "data: first\n\n");
    }

    #[test]
    fn chunk_decoder_rejects_an_extra_carriage_return_in_the_terminal_header() {
        let mut decoder = ChunkDecoder::default();
        let mut wire = b"0\r\r\n\r\n".as_slice();
        assert_eq!(
            decoder.decode(&mut wire).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn chunk_decoder_accepts_a_chunk_header_with_an_extension() {
        let mut decoder = ChunkDecoder::default();
        let mut wire = b"5;ext=1\r\nhello\r\n0\r\n\r\n".as_slice();
        assert_eq!(decoder.decode(&mut wire).unwrap(), b"hello");
        assert!(decoder.decode(&mut wire).unwrap().is_empty());
        decoder.finish().unwrap();
        assert!(wire.is_empty());
    }

    #[test]
    fn chunk_decoder_accepts_one_byte_at_a_time() {
        let wire = b"3;fixture=yes\r\nabc\r\n2\r\nde\r\n0\r\nX-Fixture: trailer\r\n\r\n";
        let decode = |fragment_size| {
            let mut decoder = ChunkDecoder::default();
            let mut body = Vec::new();
            for mut fragment in wire.chunks(fragment_size) {
                while !fragment.is_empty() {
                    let before = fragment.len();
                    body.extend_from_slice(decoder.decode(&mut fragment).unwrap());
                    assert!(fragment.len() < before);
                }
            }
            assert!(decoder.is_finished());
            decoder.finish().unwrap();
            body
        };
        let whole = decode(wire.len());
        assert_eq!(whole, b"abcde");
        assert_eq!(decode(1), whole);
    }
}
