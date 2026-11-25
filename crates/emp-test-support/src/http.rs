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
    state: BodyState,
}

enum BodyState {
    CloseDelimited,
    ChunkHeader,
    ChunkData(usize),
    ChunkEnd,
    End,
}

impl<R> Body<R> {
    pub fn new(input: R, chunked: bool) -> Self {
        Self {
            input,
            state: if chunked {
                BodyState::ChunkHeader
            } else {
                BodyState::CloseDelimited
            },
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
        loop {
            match self.state {
                BodyState::CloseDelimited => return self.input.read(buffer),
                BodyState::ChunkHeader => {
                    let mut line = Vec::new();
                    read_chunk_line(&mut self.input, &mut line)?;
                    let size =
                        std::str::from_utf8(line.split(|&byte| byte == b';').next().unwrap())
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    let length = usize::from_str_radix(size.trim(), 16)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    if length == 0 {
                        loop {
                            read_chunk_line(&mut self.input, &mut line)?;
                            if trailer_is_end(&line)? {
                                break;
                            }
                        }
                        self.state = BodyState::End;
                    } else {
                        self.state = BodyState::ChunkData(length);
                    }
                }
                BodyState::ChunkData(remaining) => {
                    let length = remaining.min(buffer.len());
                    let count = self.input.read(&mut buffer[..length])?;
                    if count == 0 {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                    self.state = if count == remaining {
                        BodyState::ChunkEnd
                    } else {
                        BodyState::ChunkData(remaining - count)
                    };
                    return Ok(count);
                }
                BodyState::ChunkEnd => {
                    let mut end = [0; 2];
                    self.input.read_exact(&mut end)?;
                    if end != *b"\r\n" {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk ending"));
                    }
                    self.state = BodyState::ChunkHeader;
                }
                BodyState::End => return Ok(0),
            }
        }
    }
}

fn read_chunk_line(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<()> {
    line.clear();
    input.read_until(b'\n', line)?;
    if !line.ends_with(b"\r\n") {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    line.truncate(line.len() - 2);
    Ok(())
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
        let payload =
            "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\ndata: {";
        for (ending, error_kind) in [
            ("", io::ErrorKind::UnexpectedEof),
            ("XX", io::ErrorKind::InvalidData),
        ] {
            let wire = format!("{:x}\r\n{payload}{ending}", payload.len());
            let mut body = BufReader::new(Body::new(wire.as_bytes(), true));
            let frame = sse::read_frame(&mut body).unwrap().unwrap();
            assert_eq!(frame.event(), Some("response.completed"));
            assert_eq!(sse::read_frame(&mut body).unwrap_err().kind(), error_kind);
        }
    }
}
