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
    model: Option<String>,
    output_format: Option<String>,
    complexity: Option<String>,
    frame_count: Option<u8>,
    frame_duration_ms: Option<u16>,
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
    let client = std::sync::Arc::new(client);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    runtime.block_on(async {
        let mut calls = tokio::task::JoinSet::<(Value, Value)>::new();
        let mut pending = BTreeMap::new();
        loop {
            let (id, mut response) = tokio::select! {
                completed = calls.join_next(), if !calls.is_empty() => {
                    let Ok((id, result)) = completed.unwrap() else { continue; };
                    pending.remove(&id.to_string());
                    (id, json!({"result":result}))
                }
                line = rx.recv() => {
                    let Some(line) = line else { break; };
                    let request: Value = serde_json::from_str(&line.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                    if request["method"] == "notifications/cancelled" {
                        if let Some(handle) = pending.remove(&request["params"]["requestId"].to_string()) {
                            let handle: tokio::task::AbortHandle = handle;
                            handle.abort();
                        }
                        continue;
                    }
                    let Some(id) = request.get("id").cloned() else { continue; };
                    let response = match request["method"].as_str() {
                        Some("initialize") => json!({"result":{"protocolVersion":request["params"]["protocolVersion"],
                            "capabilities":{"tools":{}},"serverInfo":{"name":"emp-images","version":crate::VERSION}}}),
                        Some("ping") => json!({"result":{}}),
                        Some("tools/list") => json!({"result":tool_list()}),
                        Some("tools/call") if request["params"]["name"] == "imagegen" => {
                            let args = serde_json::from_value::<ImageArgs>(request["params"]["arguments"].clone());
                            let client = client.clone();
                            let base_url = base_url.to_owned();
                            let key = key.trim().to_owned();
                            let output = output.clone();
                            let call_id = id.clone();
                            let handle = calls.spawn(async move {
                                let result = match args {
                                    Ok(args) => generate(&client, &base_url, &key, &output, args,
                                        &request["params"]["_meta"]).await,
                                    Err(error) => Err(error.to_string()),
                                };
                                let result = match result {
                                    Ok(path) => json!({"content":[{"type":"text","text":json!({"path":path}).to_string()}]}),
                                    Err(error) => json!({"isError":true,"content":[{"type":"text","text":error}]}),
                                };
                                (call_id, result)
                            });
                            pending.insert(id.to_string(), handle);
                            continue;
                        }
                        _ => json!({"error":{"code":-32601,"message":"Unknown method or tool"}}),
                    };
                    (id, response)
                }
            };
            response["jsonrpc"] = json!("2.0");
            response["id"] = id;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout,"{response}").map_err(|e|e.to_string())?;
            stdout.flush().map_err(|e|e.to_string())?;
        }
        calls.abort_all();
        while calls.join_next().await.is_some() {}
        Ok(())
    })
}

fn tool_list() -> Value {
    json!({"tools":[{
        "name":"imagegen", "description":"Generate or edit an image using EMP's image route. Returns a local file path.",
        "inputSchema":{"type":"object","properties":{
            "prompt":{"type":"string"},
            "model":{"type":"string","enum":["gpt-image-2","gpt-image-2.6-sunburst","gpt-image-2.6-flare"]},
            "output_format":{"type":"string","enum":["png","jpeg","webp","svg","gif"]},
            "complexity":{"type":"string","enum":["low","medium","high"]},
            "frame_count":{"type":"integer","minimum":2,"maximum":16},
            "frame_duration_ms":{"type":"integer","minimum":20,"maximum":2000},
            "referenced_image_paths":{"type":"array","items":{"type":"string"},"description":"Local PNG, JPEG, WebP or GIF paths for editing."},
            "transparent_background":{"type":"boolean"}},"required":["prompt"],"additionalProperties":false},
        "annotations":{"destructiveHint":false}}]})
}

async fn generate(
    client: &HttpClient,
    base_url: &str,
    key: &str,
    output: &Path,
    args: ImageArgs,
    meta: &Value,
) -> Result<PathBuf, String> {
    let format = args.output_format.as_deref().unwrap_or("png");
    if !matches!(format, "png" | "jpeg" | "webp" | "svg" | "gif") {
        return Err("Unsupported image output format".into());
    }
    if format == "svg" && !args.referenced_image_paths.is_empty() {
        return Err("SVG output supports generation only".into());
    }
    let mut body = json!({"model":args.model.as_deref().unwrap_or("gpt-image-2"),"prompt":args.prompt,"quality":"auto","size":"auto",
        "background":if args.transparent_background {"transparent"} else {"opaque"}});
    body["output_format"] = json!(format);
    for (name, value) in [
        ("complexity", json!(args.complexity)),
        ("frame_count", json!(args.frame_count)),
        ("frame_duration_ms", json!(args.frame_duration_ms)),
    ] {
        if !value.is_null() {
            body[name] = value;
        }
    }
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
        "emp-{}.{format}",
        crate::util::random_hex(12).map_err(|error| error.to_string())?
    ));
    emp_state::atomic_write_private_state(&path, &image).map_err(|error| error.to_string())?;
    Ok(path)
}
