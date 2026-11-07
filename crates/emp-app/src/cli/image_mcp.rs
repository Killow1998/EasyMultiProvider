//! One local image tool; files stay on the client and inference uses the gateway.
use base64::{Engine, engine::general_purpose::STANDARD};
use emp_transport::{HttpClient, HttpClientPolicy, HttpMethod, ProxyPolicy, TimeoutPolicy};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct ImageArgs {
    prompt: String,
    #[serde(default)]
    referenced_image_paths: Vec<PathBuf>,
    #[serde(default)]
    transparent_background: bool,
}

pub(crate) fn run(base_url: &str, key_file: &Path) -> Result<(), String> {
    let key = std::fs::read_to_string(key_file).map_err(|error| error.to_string())?;
    let output = key_file.parent().unwrap().join("generated_images");
    let client = HttpClient::new(HttpClientPolicy::new(
        ProxyPolicy::explicit(None),
        TimeoutPolicy::default(),
    ))
    .map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        let Some(id) = request.get("id") else {
            continue;
        };
        let result = match request["method"].as_str() {
            Some("initialize") => json!({"protocolVersion":request["params"]["protocolVersion"],
                "capabilities":{"tools":{}},"serverInfo":{"name":"emp-images","version":crate::VERSION}}),
            Some("ping") => json!({}),
            Some("tools/list") => json!({"tools":[{
                "name":"imagegen", "description":"Generate or edit an image using EMP's image route. Returns a local PNG path.",
                "inputSchema":{"type":"object","properties":{
                    "prompt":{"type":"string"},
                    "referenced_image_paths":{"type":"array","items":{"type":"string"},"description":"Local PNG, JPEG, WebP or GIF paths for editing."},
                    "transparent_background":{"type":"boolean"}},"required":["prompt"],"additionalProperties":false},
                "annotations":{"destructiveHint":false}}]}),
            Some("tools/call") if request["params"]["name"] == "imagegen" => {
                let args =
                    serde_json::from_value::<ImageArgs>(request["params"]["arguments"].clone())
                        .map_err(|error| error.to_string());
                let result = args.and_then(|args| {
                    runtime.block_on(generate(
                        &client,
                        base_url,
                        key.trim(),
                        &output,
                        args,
                        &request["params"]["_meta"],
                    ))
                });
                match result {
                    Ok(path) => {
                        json!({"content":[{"type":"text","text":json!({"path":path}).to_string()}]})
                    }
                    Err(error) => json!({"isError":true,"content":[{"type":"text","text":error}]}),
                }
            }
            _ => {
                writeln!(stdout,"{}",json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unknown method or tool"}})).map_err(|error| error.to_string())?;
                stdout.flush().map_err(|error| error.to_string())?;
                continue;
            }
        };
        writeln!(
            stdout,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"result":result})
        )
        .map_err(|error| error.to_string())?;
        stdout.flush().map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn generate(
    client: &HttpClient,
    base_url: &str,
    key: &str,
    output: &Path,
    args: ImageArgs,
    meta: &Value,
) -> Result<PathBuf, String> {
    let mut body = json!({"model":"gpt-image-2","prompt":args.prompt,"quality":"auto","size":"auto",
        "background":if args.transparent_background {"transparent"} else {"opaque"}});
    let mut images = Vec::new();
    for path in args.referenced_image_paths {
        let bytes = emp_state::read_file_limited(&path, 32 * 1024 * 1024)
            .map_err(|error| error.to_string())?;
        let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err("Unsupported reference image; use PNG, JPEG, WebP or GIF".into());
        };
        images.push(json!({"image_url":format!("data:{mime};base64,{}",STANDARD.encode(bytes))}));
    }
    let operation = if images.is_empty() {
        "generations"
    } else {
        body["images"] = json!(images);
        "edits"
    };
    let mut headers = BTreeMap::from([
        ("Authorization".into(), format!("Bearer {key}")),
        ("Content-Type".into(), "application/json".into()),
    ]);
    let metadata = meta
        .get("x-codex-turn-metadata")
        .ok_or("Image generation requires Codex turn metadata")?;
    let metadata: Value = match metadata.as_str() {
        Some(encoded) => serde_json::from_str(encoded).map_err(|error| error.to_string())?,
        None => metadata.clone(),
    };
    let turn_id = metadata["turn_id"]
        .as_str()
        .ok_or("Image generation requires a Codex turn ID")?;
    headers.insert("x-codex-image-turn-id".into(), turn_id.to_owned());
    headers.insert("x-codex-turn-metadata".into(), metadata.to_string());
    let response = client
        .open(
            HttpMethod::Post,
            &format!("{}/images/{operation}", base_url.trim_end_matches('/')),
            headers,
            Some(serde_json::to_vec(&body).unwrap()),
            false,
        )
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let bytes = response
        .read_limited(64 * 1024 * 1024)
        .await
        .map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "EMP image request failed ({status}): {}",
            value["error"]["message"]
                .as_str()
                .unwrap_or("Image service rejected the request")
        ));
    }
    let encoded = value["data"][0]["b64_json"]
        .as_str()
        .ok_or("EMP returned no image")?;
    let image = STANDARD
        .decode(encoded)
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(output).map_err(|error| error.to_string())?;
    let path = output.join(format!(
        "emp-{}.png",
        crate::util::random_hex(12).map_err(|error| error.to_string())?
    ));
    emp_state::atomic_write_private_state(&path, &image).map_err(|error| error.to_string())?;
    Ok(path)
}
