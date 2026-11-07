use super::*;

const TINY_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

#[test]
fn original_images_and_large_text_keep_their_content_without_stdin_carriers() {
    let body = json!({"input":[{"role":"user","content":[
        {"type":"input_image","image_url":format!("data:image/png;base64,{TINY_PNG}")},
        {"type":"input_image","image_url":"https://images.invalid/original.png"}
    ]}]});
    let prepared = prepare(&body).unwrap();
    let ExpectedUserContent::Blocks(blocks) = prepared.expected_user_content else {
        panic!("native blocks");
    };
    assert_eq!(
        context_payload(&body).unwrap()["messages"][0]["content"],
        json!(blocks)
    );
    let images = blocks
        .iter()
        .filter(|block| block["type"] == "image")
        .collect::<Vec<_>>();
    assert_eq!(images[0]["source"]["data"], TINY_PNG);
    assert_eq!(
        images[1]["source"]["url"],
        "https://images.invalid/original.png"
    );
    let text = "line\\n\\\"quoted\\\" 🧪 ".repeat(1_000_000);
    let prepared = prepare(&json!({"input":text})).unwrap();
    let ExpectedUserContent::Text(encoded) = prepared.expected_user_content else {
        panic!("text");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&encoded).unwrap()["input"],
        text
    );
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
    let event = serde_json::to_value(&prepared.expected_user_content).unwrap();
    let blocks = event.as_array().expect("content blocks");
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
    let event = serde_json::to_value(&prepared.expected_user_content).unwrap();
    let image = event
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
    let event = serde_json::to_value(&prepared.expected_user_content).unwrap();
    let blocks = event.as_array().expect("content");
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
        assessment
            .input_estimate
            .is_some_and(|estimate| { estimate >= u64::try_from(encoded_pdf.len() / 2).unwrap() })
    );
}
