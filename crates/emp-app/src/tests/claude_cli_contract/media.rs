//! Generated media fixtures and installed-CLI image/document contracts.
use super::*;

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_forwards_images_and_documents_with_ordered_tool_history() {
    assert_cli_forwards_images_and_documents_with_ordered_tool_history("sonnet");
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_opus_55_forwards_images_and_documents_with_ordered_tool_history() {
    assert_cli_forwards_images_and_documents_with_ordered_tool_history("claude-opus-5-5");
}

fn assert_cli_forwards_images_and_documents_with_ordered_tool_history(model: &str) {
    assert!(
        emp_codex::installed_cli::resolve_claude_cli().is_some(),
        "trusted Claude Code CLI must be on PATH"
    );

    let screenshot = synthetic_screenshot_png();
    let screenshot_base64 = STANDARD.encode(&screenshot);
    let inline = format!("data:image/png;base64,{screenshot_base64}");
    let pdf_bytes = minimal_pdf();
    let pdf_data = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);
    let text_file_bytes = b"tool text document";
    let pdf = format!("data:application/pdf;base64,{pdf_data}");
    let text_file = format!(
        "data:text/plain;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(text_file_bytes)
    );
    let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
        "answer":"multimodal path accepted", "tool_calls":[]
    }))]);
    let (_directory, server) = claude_server_with_model(&upstream.base_url(), model);
    let request_body = json!({
        "model":"demo/model",
        "stream":false,
        "reasoning":{"effort":"low"},
        "input":[
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"Inspect this inline image."},
                {"type":"input_image","image_url":inline,"detail":"high"},
                {"type":"input_file","filename":"tiny.pdf","file_data":pdf}
            ]},
            {"type":"function_call","call_id":"call-image-tool","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-image-tool","output":[
                {"type":"input_text","text":"ordinary payload JSON: {\"type\":\"input_image\",\"reasoning\":\"keep this field\"}"},
                {"type":"input_image","image_url":"https://images.invalid/tool-output.png"},
                {"type":"input_file","filename":"note.txt","file_data":text_file}
            ]},
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"Use the image and tool history for the follow-up."}
            ]}
        ],
        "tools":[{"type":"function","name":"inspect","parameters":{"type":"object","properties":{}}}]
    });
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&request_body).expect("multimodal Responses JSON"),
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).expect("Responses JSON");
    assert_eq!(
        crate::services::compaction::response_output_text(&result).as_deref(),
        Some("multimodal path accepted")
    );

    let (path, headers, messages_request) = upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(messages_request["model"], model);
    assert_eq!(messages_request["stream"], true);
    assert_eq!(messages_request["messages"].as_array().unwrap().len(), 1);
    assert_eq!(messages_request["messages"][0]["role"], "user");
    // Bare CLI versions can omit the date reminder and send only empty
    // system-role effort metadata. The EMP bridge instruction must remain.
    assert!(
        messages_request["system"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| {
                block["text"].as_str().is_some_and(|text| {
                    text.starts_with("You answer the next turn for the Codex runtime.")
                })
            })
    );
    assert_eq!(messages_request["tools"].as_array().unwrap().len(), 1);
    assert_eq!(messages_request["tools"][0]["name"], "StructuredOutput");

    let content = messages_request["messages"][0]["content"]
        .as_array()
        .expect("ordered text and native media blocks");
    let images = content
        .iter()
        .enumerate()
        .filter(|(_, block)| block["type"] == "image")
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 2, "both input images must reach CPA natively");
    assert_eq!(images[0].1["source"]["type"], "base64");
    assert_eq!(images[0].1["source"]["media_type"], "image/png");
    assert_eq!(
        images[0].1["source"]["data"], screenshot_base64,
        "the provider must receive the original screenshot bytes"
    );
    assert!(!content.iter().any(|block| {
        block["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("[Image: original "))
    }));
    assert_eq!(images[1].1["source"]["type"], "url");
    assert_eq!(
        images[1].1["source"]["url"],
        "https://images.invalid/tool-output.png"
    );
    let documents = content
        .iter()
        .enumerate()
        .filter(|(_, block)| block["type"] == "document")
        .collect::<Vec<_>>();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].1["source"]["type"], "base64");
    assert_eq!(documents[0].1["source"]["media_type"], "application/pdf");
    assert_eq!(
        documents[0].1["source"]["data"],
        base64::engine::general_purpose::STANDARD.encode(&pdf_bytes),
        "the CLI must preserve PDF bytes"
    );

    for (block_index, block) in content.iter().enumerate() {
        let Some(record_text) = block["text"].as_str() else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(record_text) else {
            continue;
        };
        if record["content_part"]["type"] == "input_image"
            || record["tool_output_part"]["type"] == "input_image"
        {
            assert_eq!(content[block_index + 1]["type"], "image");
        }
        if record["content_part"]["type"] == "input_file"
            && record["content_part"]["filename"] == "tiny.pdf"
        {
            assert_eq!(content[block_index + 1]["type"], "document");
        }
        if record["tool_output_part"]["type"] == "input_file"
            && record["tool_output_part"]["filename"] == "note.txt"
        {
            assert_eq!(record["item"]["call_id"], "call-image-tool");
            assert_eq!(record["tool_output_part_index"], 2);
            assert_eq!(content[block_index + 1]["type"], "text");
            assert_eq!(
                content[block_index + 1]["text"],
                "Document note.txt:\ntool text document"
            );
        }
        if record["codex_item_index"] == 0 && record["content_part"]["type"] == "input_image" {
            assert_eq!(record["item"]["role"], "user");
            assert_eq!(record["content_part_index"], 1);
        }
        if record["item"]["call_id"] == "call-image-tool"
            && record["tool_output_part"]["type"] == "input_image"
        {
            assert_eq!(record["tool_output_part_index"], 1);
        }
        assert!(
            !record_text.contains(screenshot_base64.as_str()),
            "image bytes stay in the native block"
        );
    }
    assert!(content.iter().any(|block| {
        block["text"].as_str().is_some_and(|text| {
            text.contains("ordinary payload JSON") && text.contains("keep this field")
        })
    }));

    server.shutdown().expect("shutdown multimodal server");
}

pub(super) fn minimal_pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] /Contents 4 0 R >>",
        "<< /Length 0 >>\nstream\nendstream",
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0_usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets.into_iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    pdf
}

fn synthetic_screenshot_png() -> Vec<u8> {
    synthetic_png(2560, 1440, false)
}

pub(super) fn synthetic_png(width: u32, height: u32, rgba: bool) -> Vec<u8> {
    let mut scanlines = Vec::new();
    for y in 0..height {
        scanlines.push(0);
        for x in 0..width {
            if rgba {
                scanlines.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 255]);
            } else {
                scanlines.push((((x / 24) ^ (y / 20)) & 0xff) as u8);
            }
        }
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(&scanlines)
        .expect("compress screenshot scanlines");
    let image_data = encoder.finish().expect("finish screenshot compression");

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, if rgba { 6 } else { 0 }, 0, 0, 0]);
    append_png_chunk(&mut png, *b"IHDR", &header);
    append_png_chunk(&mut png, *b"IDAT", &image_data);
    append_png_chunk(&mut png, *b"IEND", &[]);
    png
}

fn append_png_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let mut checksum_data = Vec::with_capacity(kind.len() + data.len());
    checksum_data.extend_from_slice(&kind);
    checksum_data.extend_from_slice(data);
    png.extend_from_slice(&png_crc32(&checksum_data).to_be_bytes());
}

fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}
