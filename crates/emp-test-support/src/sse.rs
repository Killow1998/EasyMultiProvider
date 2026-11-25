use std::io::{self, BufRead};

#[derive(Debug)]
pub struct Frame(String);

impl Frame {
    pub fn raw(&self) -> &str {
        &self.0
    }

    pub fn event(&self) -> Option<&str> {
        self.field_values("event").last()
    }

    pub fn data(&self) -> Option<String> {
        let mut lines = self.field_values("data");
        let mut data = lines.next()?.to_owned();
        for line in lines {
            data.push('\n');
            data.push_str(line);
        }
        Some(data)
    }

    pub fn is_comment_or_empty(&self) -> bool {
        self.raw()
            .lines()
            .all(|line| line.is_empty() || line.starts_with(':'))
    }

    fn field_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.raw().lines().filter_map(move |line| {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            (field == name).then(|| value.strip_prefix(' ').unwrap_or(value))
        })
    }
}

pub fn read_frame(input: &mut impl BufRead) -> io::Result<Option<Frame>> {
    let mut raw = Vec::new();
    loop {
        let line_start = raw.len();
        if input.read_until(b'\n', &mut raw)? == 0 {
            return if raw.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unfinished SSE frame",
                ))
            };
        }
        if matches!(&raw[line_start..], b"\n" | b"\r\n") {
            return String::from_utf8(raw)
                .map(|raw| Some(Frame(raw)))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
}

pub fn frames(mut input: &[u8]) -> impl Iterator<Item = io::Result<Frame>> {
    std::iter::from_fn(move || read_frame(&mut input).transpose())
}

pub fn first_business_frame(input: &mut impl BufRead) -> io::Result<Option<Frame>> {
    while let Some(frame) = read_frame(input)? {
        if !frame.is_comment_or_empty() {
            return Ok(Some(frame));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(input: &str) -> Frame {
        read_frame(&mut input.as_bytes()).unwrap().unwrap()
    }

    #[test]
    fn data_joins_fields_with_newlines() {
        assert_eq!(
            frame("data: first\ndata: second\n\n").data().as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn data_ignores_unprefixed_continuation() {
        let frame = frame("event: response.failed\ndata: {\n\"type\":\"response.failed\"}\n\n");
        assert_eq!(frame.event(), Some("response.failed"));
        assert_eq!(frame.data().as_deref(), Some("{"));
    }

    #[test]
    fn fields_accept_values_with_or_without_one_space() {
        assert_eq!(frame("event:created\n\n").event(), Some("created"));
        assert_eq!(frame("event: created\n\n").event(), Some("created"));
        assert_eq!(
            frame("data:first\ndata: second\ndata:  third\n\n")
                .data()
                .as_deref(),
            Some("first\nsecond\n third")
        );
    }

    #[test]
    fn fields_ignore_comments() {
        let mixed = frame(":event: ignored\nevent: updated\n:data: ignored\ndata: kept\n\n");
        assert_eq!(mixed.event(), Some("updated"));
        assert_eq!(mixed.data().as_deref(), Some("kept"));
        assert!(!mixed.is_comment_or_empty());
        let comment = frame(":event: ignored\n:data: ignored\n\n");
        assert_eq!(comment.event(), None);
        assert_eq!(comment.data(), None);
        assert!(comment.is_comment_or_empty());
    }

    #[test]
    fn rejects_unfinished_tail_after_delivering_complete_frames() {
        let mut frames =
            frames(b"data: first\n\ndata: {\ndata: \"type\":\"response.completed\"}\n");
        assert_eq!(frames.next().unwrap().unwrap().raw(), "data: first\n\n");
        assert_eq!(
            frames.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(frames.next().is_none());
    }

    #[test]
    fn first_business_frame_skips_comments_and_empty_frames() {
        let mut input =
            b": keepalive\n\n\nevent:response.failed\ndata:{}\n\ndata: next\n\n".as_slice();
        let frame = first_business_frame(&mut input).unwrap().unwrap();
        assert_eq!(frame.event(), Some("response.failed"));
        assert_eq!(frame.data().as_deref(), Some("{}"));
        assert_eq!(input, b"data: next\n\n");
    }

    #[test]
    fn reads_multiple_frames_with_original_line_endings() {
        let crlf = "event: first\r\ndata: 回答\r\ndata: continuation\r\n\r\n";
        let lf = "event: second\ndata: value\n\n";
        let input = format!("{crlf}{lf}");
        let frames = frames(input.as_bytes())
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].raw(), crlf);
        assert_eq!(frames[0].event(), Some("first"));
        assert_eq!(frames[0].data().as_deref(), Some("回答\ncontinuation"));
        assert_eq!(frames[1].raw(), lf);
        assert_eq!(frames[1].event(), Some("second"));
        assert_eq!(frames[1].data().as_deref(), Some("value"));
    }
}
