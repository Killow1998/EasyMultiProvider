//! Strict, bounded multipart parser for Codex Voice call creation.

use super::{
    MAX_REALTIME_PART_BYTES, MAX_REALTIME_PART_HEADER_BYTES, MAX_REALTIME_REQUEST_BYTES,
    RealtimeCall, RealtimeError,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

pub(crate) fn read_realtime_call<R: Read>(
    headers: &BTreeMap<String, String>,
    mut prefix: Vec<u8>,
    stream: &mut R,
) -> Result<RealtimeCall, RealtimeError> {
    let content_encoding = header(headers, "content-encoding").unwrap_or("identity");
    if !content_encoding.trim().eq_ignore_ascii_case("identity") {
        return Err(RealtimeError::new(
            415,
            "realtime_unsupported_encoding",
            "Realtime calls require identity Content-Encoding",
        )
        .closing());
    }
    let content_type = header(headers, "content-type").unwrap_or_default();
    if media_type(content_type).as_deref() != Some("multipart/form-data") {
        return Err(RealtimeError::new(
            415,
            "realtime_invalid_content_type",
            "Content-Type must be multipart/form-data for /v1/live",
        )
        .closing());
    }
    let boundary = multipart_boundary(content_type).ok_or_else(|| {
        RealtimeError::new(
            400,
            "realtime_invalid_multipart",
            "Realtime multipart boundary is missing or invalid",
        )
        .closing()
    })?;

    if header(headers, "transfer-encoding").is_some() {
        return Err(RealtimeError::new(
            400,
            "realtime_invalid_request",
            "Transfer-Encoding is not supported for realtime calls",
        )
        .closing());
    }
    let raw_length = header(headers, "content-length").ok_or_else(|| {
        RealtimeError::new(
            411,
            "realtime_length_required",
            "Content-Length is required for realtime calls",
        )
        .closing()
    })?;
    let raw_length = raw_length.trim();
    if let Some(negative) = raw_length.strip_prefix('-') {
        if negative.is_empty() || !negative.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(RealtimeError::new(
                400,
                "realtime_invalid_request",
                "invalid Content-Length",
            )
            .closing());
        }
        return Err(RealtimeError::new(
            400,
            "realtime_invalid_request",
            "Content-Length cannot be negative",
        )
        .closing());
    }
    let positive = raw_length.strip_prefix('+').unwrap_or(raw_length);
    if positive.is_empty() || !positive.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(
            RealtimeError::new(400, "realtime_invalid_request", "invalid Content-Length").closing(),
        );
    }
    let length = positive.parse::<u128>().unwrap_or(u128::MAX);
    if length > MAX_REALTIME_REQUEST_BYTES as u128 {
        return Err(RealtimeError::new(
            413,
            "realtime_request_too_large",
            format!("Realtime request exceeds the {MAX_REALTIME_REQUEST_BYTES} byte limit"),
        )
        .closing());
    }
    let length = length as usize;
    if prefix.len() > length {
        prefix.truncate(length);
    }
    prefix
        .try_reserve_exact(length.saturating_sub(prefix.len()))
        .map_err(|_| {
            RealtimeError::new(
                413,
                "realtime_request_too_large",
                format!("Realtime request exceeds the {MAX_REALTIME_REQUEST_BYTES} byte limit"),
            )
            .closing()
        })?;
    while prefix.len() < length {
        let remaining = length - prefix.len();
        let mut chunk = [0u8; 32 * 1024];
        let chunk_length = remaining.min(chunk.len());
        let count = stream.read(&mut chunk[..chunk_length]).map_err(|_| {
            RealtimeError::new(
                400,
                "realtime_invalid_request",
                "Realtime request body is incomplete",
            )
            .closing()
        })?;
        if count == 0 {
            return Err(RealtimeError::new(
                400,
                "realtime_invalid_request",
                "Realtime request body is incomplete",
            )
            .closing());
        }
        prefix.extend_from_slice(&chunk[..count]);
    }
    parse_realtime_multipart(&prefix, &boundary)
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn media_type(raw: &str) -> Option<String> {
    raw.split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| value.is_ascii())
        .map(str::to_ascii_lowercase)
}

fn multipart_boundary(raw: &str) -> Option<String> {
    for parameter in raw.split(';').skip(1) {
        let (name, value) = parameter.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let value = value.trim();
        let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if valid_boundary(value) {
            return Some(value.to_owned());
        }
        return None;
    }
    None
}

fn valid_boundary(value: &str) -> bool {
    (1..=70).contains(&value.len())
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn parse_realtime_multipart(raw: &[u8], boundary: &str) -> Result<RealtimeCall, RealtimeError> {
    if raw.len() > MAX_REALTIME_REQUEST_BYTES {
        return Err(RealtimeError::new(
            413,
            "realtime_request_too_large",
            format!("Realtime request exceeds the {MAX_REALTIME_REQUEST_BYTES} byte limit"),
        ));
    }
    if !valid_boundary(boundary) {
        return Err(invalid_multipart("Realtime multipart boundary is invalid"));
    }
    let delimiter = [b"--".as_slice(), boundary.as_bytes()].concat();
    let mut parts = Vec::new();
    let mut start = 0;
    while let Some(offset) = find_subslice(&raw[start..], &delimiter) {
        let end = start + offset;
        parts.push(&raw[start..end]);
        start = end + delimiter.len();
        if start >= raw.len() {
            break;
        }
    }
    parts.push(&raw[start..]);
    if parts.len() != 4 || !parts[0].is_empty() || !(parts[3] == b"--" || parts[3] == b"--\r\n") {
        return Err(invalid_multipart(
            "Realtime request must contain exactly two multipart fields",
        ));
    }
    let mut fields = BTreeMap::<String, Vec<u8>>::new();
    for piece in &parts[1..3] {
        if !piece.starts_with(b"\r\n") || !piece.ends_with(b"\r\n") {
            return Err(invalid_multipart("Realtime multipart framing is invalid"));
        }
        let part = &piece[2..piece.len() - 2];
        let Some(header_end) = find_subslice(part, b"\r\n\r\n") else {
            return Err(invalid_multipart("Realtime multipart headers are invalid"));
        };
        if header_end > MAX_REALTIME_PART_HEADER_BYTES {
            return Err(invalid_multipart("Realtime multipart headers are invalid"));
        }
        let payload = &part[header_end + 4..];
        if payload.len() > MAX_REALTIME_PART_BYTES {
            return Err(RealtimeError::new(
                413,
                "realtime_part_too_large",
                format!(
                    "Realtime multipart field exceeds the {MAX_REALTIME_PART_BYTES} byte limit"
                ),
            ));
        }
        let header_lines = split_crlf(&part[..header_end]);
        if header_lines.len() > 4 {
            return Err(invalid_multipart(
                "Realtime multipart field has too many headers",
            ));
        }
        let mut part_headers = BTreeMap::new();
        for line in header_lines {
            if line.contains(&b'\r') || line.contains(&b'\n') {
                return Err(invalid_multipart("Realtime multipart header is malformed"));
            }
            let Some(colon) = line.iter().position(|byte| *byte == b':') else {
                return Err(invalid_multipart("Realtime multipart header is malformed"));
            };
            if line
                .first()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
            {
                return Err(invalid_multipart("Realtime multipart header is malformed"));
            }
            let name = std::str::from_utf8(&line[..colon])
                .ok()
                .filter(|name| name.is_ascii())
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| invalid_multipart("Realtime multipart headers must be ASCII"))?;
            let value = std::str::from_utf8(&line[colon + 1..])
                .ok()
                .filter(|value| value.is_ascii())
                .map(str::trim)
                .map(str::to_owned)
                .ok_or_else(|| invalid_multipart("Realtime multipart headers must be ASCII"))?;
            if !matches!(name.as_str(), "content-disposition" | "content-type") {
                return Err(invalid_multipart(
                    "Realtime multipart field contains an unsupported header",
                ));
            }
            if part_headers.insert(name, value).is_some() {
                return Err(invalid_multipart(
                    "Realtime multipart field contains a duplicate header",
                ));
            }
        }
        let disposition = part_headers
            .get("content-disposition")
            .and_then(|value| parse_disposition(value))
            .ok_or_else(|| {
                invalid_multipart("Realtime multipart field requires form-data disposition")
            })?;
        let field_name = disposition;
        let expected_type = if field_name == "sdp" {
            "application/sdp"
        } else {
            "application/json"
        };
        if !matches!(field_name.as_str(), "sdp" | "session") {
            return Err(invalid_multipart(
                "Realtime multipart field name is not allowed",
            ));
        }
        let field_type = part_headers
            .get("content-type")
            .and_then(|value| media_type(value))
            .unwrap_or_default();
        if field_type != expected_type {
            return Err(RealtimeError::new(
                415,
                "realtime_invalid_part_content_type",
                format!("Realtime {field_name} field must use {expected_type}"),
            ));
        }
        if fields.insert(field_name, payload.to_vec()).is_some() {
            return Err(invalid_multipart(
                "Realtime multipart fields must not be duplicated",
            ));
        }
    }
    if fields.len() != 2 || !fields.contains_key("sdp") || !fields.contains_key("session") {
        return Err(invalid_multipart(
            "Realtime request requires sdp and session fields",
        ));
    }
    let sdp =
        String::from_utf8(fields.remove("sdp").expect("validated SDP field")).map_err(|_| {
            invalid_multipart("Realtime sdp and session fields must contain valid UTF-8 data")
        })?;
    let session: Value =
        serde_json::from_slice(&fields.remove("session").expect("validated session field"))
            .map_err(|_| {
                invalid_multipart("Realtime sdp and session fields must contain valid UTF-8 data")
            })?;
    if !sdp.starts_with("v=0") || sdp.contains('\0') {
        return Err(RealtimeError::new(
            400,
            "realtime_invalid_sdp",
            "Realtime SDP offer is empty or invalid",
        ));
    }
    if !session.is_object() {
        return Err(RealtimeError::new(
            400,
            "realtime_invalid_session",
            "Realtime session must be a JSON object",
        ));
    }
    Ok(RealtimeCall { sdp, session })
}

fn parse_disposition(raw: &str) -> Option<String> {
    let mut segments = raw.split(';');
    if !segments.next()?.trim().eq_ignore_ascii_case("form-data") {
        return None;
    }
    let parameter = segments.next()?.trim();
    if segments.next().is_some() {
        return None;
    }
    let (name, value) = parameter.split_once('=')?;
    if !name.trim().eq_ignore_ascii_case("name") {
        return None;
    }
    let value = value.trim();
    let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        &value[1..value.len() - 1]
    } else {
        value
    };
    Some(value.to_owned())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn split_crlf(raw: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(offset) = find_subslice(&raw[start..], b"\r\n") {
        let end = start + offset;
        lines.push(&raw[start..end]);
        start = end + 2;
    }
    lines.push(&raw[start..]);
    lines
}

fn invalid_multipart(message: &str) -> RealtimeError {
    RealtimeError::new(400, "realtime_invalid_multipart", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    const BOUNDARY: &str = "CodexRealtimeBoundary";

    fn headers(content_type: &str, length: usize) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("content-type".to_owned(), content_type.to_owned()),
            ("content-length".to_owned(), length.to_string()),
        ])
    }

    fn body(extra: bool) -> Vec<u8> {
        let mut parts = vec![
            ("sdp", "application/sdp", "v=0\r\no=offer\r\n"),
            (
                "session",
                "application/json",
                r#"{"model":"gpt-live","delegation":{"type":"client"}}"#,
            ),
        ];
        if extra {
            parts.push(("token", "text/plain", "must-not-be-forwarded"));
        }
        let mut raw = Vec::new();
        for (name, content_type, payload) in parts {
            raw.extend_from_slice(format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
            ).as_bytes());
            raw.extend_from_slice(payload.as_bytes());
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        raw
    }

    #[test]
    fn parses_exact_two_part_codex_request() {
        let raw = body(false);
        let mut stream = Cursor::new(Vec::<u8>::new());
        let call = read_realtime_call(
            &headers(
                &format!("multipart/form-data; boundary={BOUNDARY}"),
                raw.len(),
            ),
            raw,
            &mut stream,
        )
        .unwrap();
        assert_eq!(call.sdp, "v=0\r\no=offer\r\n");
        assert_eq!(
            call.session,
            json!({
                "model":"gpt-live",
                "delegation":{"type":"client"}
            })
        );
    }

    #[test]
    fn rejects_wrong_top_level_content_type() {
        let raw = body(false);
        let mut stream = Cursor::new(Vec::<u8>::new());
        let error = read_realtime_call(&headers("application/json", raw.len()), raw, &mut stream)
            .unwrap_err();
        assert_eq!(error.status, 415);
        assert_eq!(error.code, "realtime_invalid_content_type");
        assert!(error.close);
    }

    #[test]
    fn rejects_declared_request_over_limit_before_reading() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("oversized body must be rejected before reading")
            }
        }
        let mut stream = NoRead;
        let error = read_realtime_call(
            &headers(
                &format!("multipart/form-data; boundary={BOUNDARY}"),
                MAX_REALTIME_REQUEST_BYTES + 1,
            ),
            Vec::new(),
            &mut stream,
        )
        .unwrap_err();
        assert_eq!(error.status, 413);
        assert!(error.close);
    }

    #[test]
    fn rejects_extra_or_unknown_multipart_fields() {
        let raw = body(true);
        let error = parse_realtime_multipart(&raw, BOUNDARY).unwrap_err();
        assert_eq!(error.code, "realtime_invalid_multipart");
    }
}
