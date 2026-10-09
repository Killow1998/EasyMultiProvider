use super::process::InputFormat;
use super::projection;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
#[path = "json_budget.rs"]
mod json_budget;
use json_budget::JsonByteBudget;
use serde_json::{Value, json};
use url::Url;

const MAX_MEDIA_BYTES: usize = projection::MAX_TRANSCRIPT_BYTES;

pub(super) struct PreparedInput {
    pub(super) system_prompt: String,
    pub(super) stdin: Vec<u8>,
    pub(super) input_format: InputFormat,
    pub(super) expected_user_content: ExpectedUserContent,
}

pub(super) enum ExpectedUserContent {
    Text(Vec<u8>),
    Blocks(Vec<Value>),
}

enum Attachment {
    Native(Value),
    Text(String),
}

pub(super) fn preflight(body: &Value) -> Result<(), &'static str> {
    let items: Vec<&Value> = match body.get("input") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item @ Value::Object(_)) => vec![item],
        _ => Vec::new(),
    };
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str);
        if is_media_type(item_type) {
            return Err("unsupported_input_modality");
        }
        if is_message(item)
            && let Some(parts) = item.get("content").and_then(Value::as_array)
        {
            for part in parts {
                preflight_part(part)?;
            }
        } else if matches!(
            item_type,
            Some("function_call_output" | "custom_tool_call_output")
        ) && let Some(parts) = item.get("output").and_then(Value::as_array)
        {
            for part in parts {
                preflight_part(part)?;
            }
        }
    }
    Ok(())
}

fn preflight_part(part: &Value) -> Result<(), &'static str> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_audio" | "input_video") => Err("unsupported_input_modality"),
        Some("input_image") => {
            if part.get("file_id").is_some() {
                return Err("unsupported_input_modality");
            }
            let url = match part.get("image_url") {
                Some(Value::String(url)) => url.as_str(),
                Some(Value::Object(image)) => image
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or("unsupported_input_modality")?,
                _ => return Err("unsupported_input_modality"),
            };
            if let Some((media_type, payload)) = data_url_header(url)? {
                if !matches!(
                    media_type.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) || payload.len() > MAX_MEDIA_BYTES
                {
                    return Err("unsupported_input_modality");
                }
            } else if safe_remote_url(url).is_none() {
                return Err("unsupported_input_modality");
            }
            Ok(())
        }
        Some("input_file") => {
            if part.get("file_id").is_some() || part.get("file_url").is_some() {
                return Err("unsupported_input_modality");
            }
            let data = part
                .get("file_data")
                .and_then(Value::as_str)
                .ok_or("unsupported_input_modality")?;
            if let Some((media_type, payload)) = data_url_header(data)? {
                if !matches!(media_type.as_str(), "application/pdf" | "text/plain")
                    || payload.len() > MAX_MEDIA_BYTES
                {
                    return Err("unsupported_input_modality");
                }
            } else {
                let filename = part.get("filename").and_then(Value::as_str).unwrap_or("");
                if data.len() > MAX_MEDIA_BYTES {
                    return Err("claude_cli_input_too_large");
                }
                let media_type = if filename.to_ascii_lowercase().ends_with(".pdf") {
                    "application/pdf"
                } else if filename.to_ascii_lowercase().ends_with(".txt") {
                    "text/plain"
                } else {
                    return Err("unsupported_input_modality");
                };
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| "unsupported_input_modality")?;
                match media_type {
                    "application/pdf" if !bytes.starts_with(b"%PDF-") => {
                        return Err("unsupported_input_modality");
                    }
                    "text/plain" if std::str::from_utf8(&bytes).is_err() => {
                        return Err("unsupported_input_modality");
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(super) fn prepare(body: &Value) -> Result<PreparedInput, &'static str> {
    let prepared = prepare_input(body)?;
    if prepared.stdin.len()
        > projection::MAX_TRANSCRIPT_BYTES.saturating_sub(prepared.system_prompt.len())
    {
        return Err("claude_cli_input_too_large");
    }
    Ok(prepared)
}

fn prepare_input(body: &Value) -> Result<PreparedInput, &'static str> {
    let mut transcript = projection::normalized_transcript(body);
    let system_prompt = super::instructions::take(&mut transcript)?;
    // Text needs no per-part media markers. In particular, never expand item
    // metadata into thousands of markers only to discard them afterwards.
    match projection::transcript(&transcript) {
        Ok(stdin) => {
            return Ok(PreparedInput {
                system_prompt,
                expected_user_content: ExpectedUserContent::Text(stdin.clone()),
                stdin,
                input_format: InputFormat::Text,
            });
        }
        Err("unsupported_input_modality") => {}
        Err(error) => return Err(error),
    }
    let input = transcript.get("input");
    let items: Vec<&Value> = match input {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item @ Value::Object(_)) => vec![item],
        _ => Vec::new(),
    };
    let mut budget = JsonByteBudget::new(projection::MAX_TRANSCRIPT_BYTES);
    budget
        .charge(&json!({"type":"user","message":{"role":"user","content":[]}}))
        .map_err(|_| "claude_cli_input_too_large")?;
    budget
        .reserve(1)
        .map_err(|_| "claude_cli_input_too_large")?; // JSONL newline
    let mut content = Vec::new();
    push_block(
        &mut content,
        &mut budget,
        text_block(json!({
            "codex_responses_metadata": request_metadata(&transcript)
        }))?,
    )?;

    for (item_index, item) in items.into_iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        if is_message(item)
            && let Some(parts) = item.get("content").and_then(Value::as_array)
        {
            let metadata = object_without(item, "content");
            let before = content.len();
            for (part_index, part) in parts.iter().enumerate() {
                let attachment = attachment(part)?;
                let media_part = attachment.is_some();
                let part = marker_part(part, media_part, "native content block follows");
                push_block(
                    &mut content,
                    &mut budget,
                    text_block(json!({
                        "codex_item_index":item_index,
                        "item":metadata,
                        "content_part_index":part_index,
                        "content_part":part
                    }))?,
                )?;
                if let Some(attachment) = attachment {
                    push_block(&mut content, &mut budget, attachment_block(attachment))?;
                }
            }
            if content.len() != before {
                continue;
            }
        } else if matches!(
            item_type,
            Some("function_call_output" | "custom_tool_call_output")
        ) && let Some(parts) = item.get("output").and_then(Value::as_array)
        {
            let metadata = object_without(item, "output");
            let before = content.len();
            for (part_index, part) in parts.iter().enumerate() {
                let attachment = attachment(part)?;
                let media_part = attachment.is_some();
                let part = marker_part(part, media_part, "native tool-output block follows");
                push_block(
                    &mut content,
                    &mut budget,
                    text_block(json!({
                        "codex_item_index":item_index,
                        "item":metadata,
                        "tool_output_part_index":part_index,
                        "tool_output_part":part
                    }))?,
                )?;
                if let Some(attachment) = attachment {
                    push_block(&mut content, &mut budget, attachment_block(attachment))?;
                }
            }
            if content.len() != before {
                continue;
            }
        } else if is_media_type(item_type) {
            return Err("unsupported_input_modality");
        }

        push_block(
            &mut content,
            &mut budget,
            text_block(json!({
                "codex_item_index":item_index,
                "item":item
            }))?,
        )?;
    }

    let event = json!({
        "type":"user",
        "message":{"role":"user","content":content}
    });
    let mut stdin = serde_json::to_vec(&event).map_err(|_| "claude_cli_invalid_transcript")?;
    stdin.push(b'\n');
    if stdin.len() > projection::MAX_TRANSCRIPT_BYTES {
        return Err("claude_cli_input_too_large");
    }
    Ok(PreparedInput {
        system_prompt,
        stdin,
        input_format: InputFormat::StreamJson,
        expected_user_content: ExpectedUserContent::Blocks(content),
    })
}

/// Build the Anthropic Messages view that the CLI is expected to send, for
/// destination context assessment only. The original Responses body remains
/// untouched and is still passed to inference and compaction.
pub(super) fn context_payload(body: &Value) -> Result<Value, &'static str> {
    let prepared = prepare(body)?;
    let content = match prepared.expected_user_content {
        ExpectedUserContent::Text(bytes) => {
            Value::String(String::from_utf8(bytes).map_err(|_| "claude_cli_invalid_transcript")?)
        }
        ExpectedUserContent::Blocks(blocks) => Value::Array(blocks),
    };
    let schema_json = projection::proposal_schema()?;
    let schema: Value =
        serde_json::from_str(&schema_json).map_err(|_| "claude_cli_invalid_schema")?;
    Ok(json!({
        "system":prepared.system_prompt,
        "messages":[{"role":"user","content":content}],
        "tools":[{"name":"StructuredOutput","input_schema":schema}],
        "max_tokens":body.get("max_output_tokens").cloned().unwrap_or(Value::Null)
    }))
}

fn request_metadata(transcript: &Value) -> Value {
    object_without(transcript, "input")
}

fn is_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        || (item.get("type").is_none() && item.get("role").and_then(Value::as_str).is_some())
}

fn is_media_type(item_type: Option<&str>) -> bool {
    matches!(
        item_type,
        Some("input_image" | "input_file" | "input_audio" | "input_video")
    )
}

fn object_without(value: &Value, field: &str) -> Value {
    match value.as_object() {
        Some(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != field)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        None => value.clone(),
    }
}

fn marker_part(part: &Value, media: bool, marker: &str) -> Value {
    if !media {
        return part.clone();
    }
    let mut part = part.clone();
    if let Some(object) = part.as_object_mut() {
        match object.get("type").and_then(Value::as_str) {
            Some("input_image") => {
                object.remove("image_url");
                object.remove("file_id");
                object.insert("image_url".to_owned(), Value::String(marker.to_owned()));
            }
            Some("input_file") => {
                object.remove("file_data");
                object.remove("file_url");
                object.remove("file_id");
                object.insert("attachment".to_owned(), Value::String(marker.to_owned()));
            }
            _ => {}
        }
    }
    part
}

fn text_block(record: Value) -> Result<Value, &'static str> {
    JsonByteBudget::new(projection::MAX_TRANSCRIPT_BYTES)
        .charge(&record)
        .map_err(|_| "claude_cli_input_too_large")?;
    let text = serde_json::to_string(&record).map_err(|_| "claude_cli_invalid_transcript")?;
    Ok(json!({"type":"text","text":text}))
}

fn push_block(
    content: &mut Vec<Value>,
    budget: &mut JsonByteBudget,
    block: Value,
) -> Result<(), &'static str> {
    if !content.is_empty() {
        budget
            .reserve(1)
            .map_err(|_| "claude_cli_input_too_large")?;
    }
    budget
        .charge(&block)
        .map_err(|_| "claude_cli_input_too_large")?;
    content.push(block);
    Ok(())
}

fn attachment_block(attachment: Attachment) -> Value {
    match attachment {
        Attachment::Native(block) => block,
        Attachment::Text(text) => json!({"type":"text","text":text}),
    }
}

fn attachment(part: &Value) -> Result<Option<Attachment>, &'static str> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") => parse_image(part).map(Some),
        Some("input_file") => parse_file(part).map(Some),
        Some("input_audio" | "input_video") => Err("unsupported_input_modality"),
        _ => Ok(None),
    }
}

fn parse_image(part: &Value) -> Result<Attachment, &'static str> {
    let url = match part.get("image_url") {
        Some(Value::String(url)) => url.as_str(),
        Some(Value::Object(image)) => image
            .get("url")
            .and_then(Value::as_str)
            .ok_or("unsupported_input_modality")?,
        _ => return Err("unsupported_input_modality"),
    };
    if let Some((media_type, data)) = parse_data_url(url)? {
        if !media_type.starts_with("image/") || !valid_image(&media_type, &data) {
            return Err("unsupported_input_modality");
        }
        let encoded = STANDARD.encode(&data);
        return Ok(Attachment::Native(json!({
            "type":"image",
            "source":{"type":"base64","media_type":media_type,"data":encoded}
        })));
    }
    let parsed = safe_remote_url(url).ok_or("unsupported_input_modality")?;
    Ok(Attachment::Native(json!({
        "type":"image",
        "source":{"type":"url","url":parsed}
    })))
}

fn parse_file(part: &Value) -> Result<Attachment, &'static str> {
    let filename = part.get("filename").and_then(Value::as_str).unwrap_or("");
    let file_data = part
        .get("file_data")
        .and_then(Value::as_str)
        .ok_or("unsupported_input_modality")?;
    if let Some((media_type, data)) = parse_data_url(file_data)? {
        return file_attachment(filename, &media_type, &data);
    }
    if file_data.len() > MAX_MEDIA_BYTES {
        return Err("claude_cli_input_too_large");
    }
    let data = STANDARD
        .decode(file_data)
        .map_err(|_| "unsupported_input_modality")?;
    let media_type = if filename.to_ascii_lowercase().ends_with(".pdf") {
        "application/pdf"
    } else if filename.to_ascii_lowercase().ends_with(".txt") {
        "text/plain"
    } else {
        return Err("unsupported_input_modality");
    };
    file_attachment(filename, media_type, &data)
}

fn file_attachment(
    filename: &str,
    media_type: &str,
    data: &[u8],
) -> Result<Attachment, &'static str> {
    if data.len() > MAX_MEDIA_BYTES {
        return Err("claude_cli_input_too_large");
    }
    match media_type {
        "application/pdf" if data.starts_with(b"%PDF-") => Ok(Attachment::Native(json!({
            "type":"document",
            "source":{"type":"base64","media_type":"application/pdf","data":STANDARD.encode(data)}
        }))),
        "text/plain" => {
            let text = std::str::from_utf8(data).map_err(|_| "unsupported_input_modality")?;
            Ok(Attachment::Text(format!("Document {filename}:\n{text}")))
        }
        _ => Err("unsupported_input_modality"),
    }
}

fn parse_data_url(value: &str) -> Result<Option<(String, Vec<u8>)>, &'static str> {
    let Some((media_type, payload)) = data_url_header(value)? else {
        return Ok(None);
    };
    if payload.len() > MAX_MEDIA_BYTES {
        return Err("claude_cli_input_too_large");
    }
    let data = STANDARD
        .decode(payload)
        .map_err(|_| "unsupported_input_modality")?;
    if data.len() > MAX_MEDIA_BYTES {
        return Err("claude_cli_input_too_large");
    }
    Ok(Some((media_type, data)))
}

fn data_url_header(value: &str) -> Result<Option<(String, &str)>, &'static str> {
    let Some((scheme, rest)) = value.split_once(':') else {
        return Ok(None);
    };
    if !scheme.eq_ignore_ascii_case("data") {
        return Ok(None);
    }
    let (header, payload) = rest.split_once(',').ok_or("unsupported_input_modality")?;
    let (media_type, encoding) = header.split_once(';').ok_or("unsupported_input_modality")?;
    if !encoding.eq_ignore_ascii_case("base64") || media_type.is_empty() {
        return Err("unsupported_input_modality");
    }
    Ok(Some((media_type.to_ascii_lowercase(), payload)))
}

fn safe_remote_url(value: &str) -> Option<&str> {
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(value)
}

fn valid_image(media_type: &str, data: &[u8]) -> bool {
    match media_type {
        "image/png" => png_dimensions(data).is_some(),
        "image/jpeg" => data.starts_with(b"\xff\xd8\xff") && data.len() >= 4,
        "image/gif" => {
            (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) && data.len() >= 13
        }
        "image/webp" => data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

pub(super) fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") || data.get(12..16) != Some(b"IHDR") {
        return None;
    }
    let width = u32::from_be_bytes(data.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(data.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
#[path = "media_budget_tests.rs"]
mod budget_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    #[test]
    fn text_input_keeps_the_existing_text_carrier() {
        let body = json!({"model":"m","input":"hello","stream":true});
        let prepared = prepare(&body).expect("text input");
        assert!(matches!(prepared.input_format, InputFormat::Text));
        assert_eq!(prepared.stdin, projection::transcript(&body).unwrap());
        assert!(matches!(
            prepared.expected_user_content,
            ExpectedUserContent::Text(_)
        ));
    }

    #[test]
    fn images_in_messages_and_tool_output_keep_order_and_association() {
        let data_url = format!("DATA:IMAGE/PNG;BASE64,{TINY_PNG}");
        let body = json!({
            "model":"m",
            "input":[
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"inspect this"},
                    {"type":"input_image","image_url":data_url,"detail":"high"}
                ]},
                {"type":"function_call_output","call_id":"call-photo","output":[
                    {"type":"input_text","text":"opaque JSON: {\"type\":\"input_image\",\"reasoning\":\"ordinary\"}"},
                    {"type":"input_image","image_url":format!("data:image/png;base64,{TINY_PNG}")}
                ]}
            ]
        });
        let prepared = prepare(&body).expect("multimodal input");
        assert!(matches!(prepared.input_format, InputFormat::StreamJson));
        let event: Value =
            serde_json::from_slice(prepared.stdin.strip_suffix(b"\n").expect("JSONL newline"))
                .expect("stream-json event");
        assert_eq!(event["type"], "user");
        assert_eq!(event["message"]["role"], "user");
        let blocks = event["message"]["content"]
            .as_array()
            .expect("content blocks");
        let images = blocks
            .iter()
            .filter(|block| block["type"] == "image")
            .collect::<Vec<_>>();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["source"]["media_type"], "image/png");
        assert_eq!(images[1]["source"]["media_type"], "image/png");
        let markers = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>();
        assert!(markers.iter().any(|marker| {
            marker.contains("content_part_index") && marker.contains("\"role\":\"user\"")
        }));
        assert!(markers.iter().any(|marker| {
            marker.contains("call-photo") && marker.contains("tool_output_part_index")
        }));
        assert!(markers.iter().any(|marker| {
            marker.contains("opaque JSON") && marker.contains("\\\"reasoning\\\":\\\"ordinary\\\"")
        }));
        assert!(markers.iter().all(|marker| !marker.contains(TINY_PNG)));
        let ExpectedUserContent::Blocks(expected) = prepared.expected_user_content else {
            panic!("multimodal request needs exact structured relay expectation");
        };
        assert_eq!(expected, *blocks);
    }

    #[test]
    fn remote_images_are_forwarded_without_host_fetch_and_file_ids_are_rejected() {
        let body = json!({
            "input":[{"role":"user","content":[
                {"type":"input_image","image_url":"https://images.invalid/sample.png"}
            ]}]
        });
        let prepared = prepare(&body).expect("remote image");
        let event: Value =
            serde_json::from_slice(prepared.stdin.strip_suffix(b"\n").expect("JSONL newline"))
                .expect("stream-json event");
        let image = event["message"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "image")
            .expect("native URL image");
        assert_eq!(image["source"]["type"], "url");
        assert_eq!(image["source"]["url"], "https://images.invalid/sample.png");

        for unsupported in [
            json!({"type":"input_image","file_id":"file-1"}),
            json!({"type":"input_file","file_id":"file-1"}),
            json!({"type":"input_audio","input_audio":{"data":"AAAA"}}),
            json!({"type":"input_video","video_url":"https://video.invalid/a.mp4"}),
        ] {
            let body = json!({
                "input":[{"type":"message","role":"user","content":[unsupported]}]
            });
            assert_eq!(prepare(&body).err(), Some("unsupported_input_modality"));
        }
    }

    #[test]
    fn inline_pdf_and_text_files_keep_document_content_association() {
        let pdf_bytes = b"%PDF-1.4\nsynthetic fixture";
        let pdf = format!("data:application/pdf;base64,{}", STANDARD.encode(pdf_bytes));
        let text = format!(
            "data:text/plain;base64,{}",
            STANDARD.encode("document body")
        );
        let body = json!({
            "input":[
                {"type":"message","role":"user","content":[
                    {"type":"input_file","filename":"note.pdf","file_data":pdf}
                ]},
                {"type":"function_call_output","call_id":"call-document","output":[
                    {"type":"input_file","filename":"readme.txt","file_data":text}
                ]}
            ]
        });
        let prepared = prepare(&body).expect("inline documents");
        let event: Value =
            serde_json::from_slice(prepared.stdin.strip_suffix(b"\n").expect("JSONL newline"))
                .expect("stream-json event");
        let blocks = event["message"]["content"].as_array().expect("content");
        assert!(blocks.iter().any(|block| {
            block["type"] == "document"
                && block["source"]["type"] == "base64"
                && block["source"]["media_type"] == "application/pdf"
                && block["source"]["data"] == STANDARD.encode(pdf_bytes)
        }));
        assert!(blocks.iter().any(|block| {
            block["type"] == "text" && block["text"].as_str().unwrap().contains("document body")
        }));
        assert!(blocks.iter().any(|block| {
            block["text"].as_str().is_some_and(|text| {
                text.contains("call-document") && text.contains("tool_output_part_index")
            })
        }));
    }

    #[test]
    fn context_estimation_keeps_prepared_pdf_bytes_and_decoded_text() {
        let pdf_bytes = [b"%PDF-1.7\n".as_slice(), &vec![b'x'; 8192]].concat();
        let encoded_pdf = STANDARD.encode(&pdf_bytes);
        let text = "actual decoded document text ".repeat(32);
        let encoded_text = STANDARD.encode(&text);
        let body = json!({
            "model":"demo/short",
            "input":[{"type":"message","role":"user","content":[
                {"type":"input_file","filename":"large.pdf","file_data":format!("data:application/pdf;base64,{encoded_pdf}")},
                {"type":"input_file","filename":"notes.txt","file_data":format!("data:text/plain;base64,{encoded_text}")}
            ]}]
        });

        let payload = context_payload(&body).expect("CLI assessment view");
        let content = payload["messages"][0]["content"]
            .as_array()
            .expect("actual prepared CLI content");
        let document = content
            .iter()
            .find(|block| block["type"] == "document")
            .expect("native PDF block");
        assert_eq!(document["source"]["data"], encoded_pdf);
        assert!(content.iter().any(|block| {
            block["type"] == "text"
                && block["text"]
                    .as_str()
                    .is_some_and(|block_text| block_text.contains(&text))
        }));

        let provider = json!({"id":"demo"});
        let model = json!({"id":"demo/short","context_window":100000,"output_limit":64});
        let assessment = emp_history::context::assess(
            provider.as_object().unwrap(),
            model.as_object().unwrap(),
            "anthropic_messages",
            &payload,
        );
        assert!(
            assessment.input_estimate.is_some_and(|estimate| {
                estimate >= u64::try_from(encoded_pdf.len() / 2).unwrap()
            })
        );
    }
}
