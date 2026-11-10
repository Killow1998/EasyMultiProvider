use super::super::{Cancellation, ClaudeCliError, media, projection};
use super::{protocol, terminal::Terminal, transcript::Metrics};
use crate::app::ServerState;
use emp_core::ResolvedRoute;
use portable_pty::CommandBuilder;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub(super) struct Context {
    _root: tempfile::TempDir,
    command: CommandBuilder,
    listener: TcpListener,
    token: String,
    context: Option<Vec<Value>>,
    attachments: PathBuf,
    config: PathBuf,
    session: String,
    done: PathBuf,
    local: bool,
    route: ResolvedRoute,
    incoming: BTreeMap<String, String>,
    client: Arc<emp_transport::HttpClient>,
    runtime: tokio::runtime::Handle,
    shutdown: Arc<AtomicBool>,
    cancelled: Arc<Cancellation>,
}

impl Drop for Context {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

mod setup;

const CONTEXT_PROMPT_DESCRIPTION: &str = "Continue the current Codex user turn.";

pub(super) fn run(
    mut context: Context,
    input: mpsc::Receiver<super::ToolResults>,
    output: &mpsc::Sender<super::Reply>,
    metrics: &mut Metrics,
) -> Result<(), ClaudeCliError> {
    let _permit = super::super::process::admission::acquire(&context.cancelled, &mut || {
        context
            .shutdown
            .load(Ordering::Acquire)
            .then_some(super::super::process::CancellationReason::ServerShutdown)
    })
    .map_err(ClaudeCliError::Failure)?;
    let mut terminal = Terminal::spawn(context.command.clone())?;
    let mut pending: Option<(TcpStream, Value)> = None;
    let mut deadline = Instant::now() + Duration::from_secs(300);
    let mut prompt_listed = false;
    let mut submitted = false;
    loop {
        if context.cancelled.is_cancelled() {
            return Err(ClaudeCliError::Disconnected);
        }
        if context.shutdown.load(Ordering::Acquire) {
            return Err(ClaudeCliError::ShuttingDown);
        }
        if Instant::now() >= deadline {
            return Err(ClaudeCliError::Failure("claude_cli_timeout"));
        }
        // Discovery returns before the CLI registers commands; autocomplete acknowledges readiness.
        let prompt_ready = terminal.pump(CONTEXT_PROMPT_DESCRIPTION.as_bytes())?;
        if prompt_listed && !submitted && prompt_ready {
            terminal.write(b"\r")?;
            submitted = true;
        }
        if context.local {
            metrics
                .drain(&context.config, &context.session)
                .map_err(|_| ClaudeCliError::Failure("claude_cli_invalid_output"))?;
        }
        if context.done.is_file() {
            let text = metrics.text();
            output
                .send(super::Reply::Complete(
                    metrics.output(json!({"answer":text,"tool_calls":[]})),
                    Ok(()),
                ))
                .map_err(|_| ClaudeCliError::Disconnected)?;
            return Ok(());
        }
        match input.try_recv() {
            Ok(result) => {
                let (mut stream, id) = pending
                    .take()
                    .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_history"))?;
                let prepared = media::prepare(&json!({"input":result.input,"tools":result.tools}))
                    .map_err(ClaudeCliError::Failure)?;
                let result = super::attachments::prepare(
                    prepared.expected_user_content,
                    context.local,
                    &context.attachments,
                )?;
                protocol::reply(
                    &mut stream,
                    200,
                    &json!({"jsonrpc":"2.0","id":id,"result":{"content":result,"isError":false}}),
                )?;
                if context.local {
                    output
                        .send(super::Reply::Dispatched)
                        .map_err(|_| ClaudeCliError::Disconnected)?;
                }
                deadline = Instant::now() + Duration::from_secs(300);
            }
            Err(mpsc::TryRecvError::Disconnected) => return Err(ClaudeCliError::Disconnected),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let (mut stream, _) = match context.listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => return Err(ClaudeCliError::Failure("claude_cli_relay_unavailable")),
        };
        let (path, mut request, headers) = protocol::read(&mut stream, &context.token)?;
        if path == "/v1/messages" && !context.local {
            super::attachments::restore(&mut request, &context.attachments)?;
            output
                .send(super::Reply::Dispatched)
                .map_err(|_| ClaudeCliError::Disconnected)?;
            let router = emp_router::ExternalRouter::new(&context.client);
            let mut message_id = String::new();
            let mut has_text = false;
            let mut observe = |event: &Value| {
                let text = match event["type"].as_str() {
                    Some("message_start") => {
                        has_text = false;
                        message_id = event["message"]["id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        None
                    }
                    Some("content_block_start") if event["content_block"]["type"] == "text" => {
                        if has_text {
                            let _ = output
                                .send(super::Reply::Text(message_id.clone(), "\n".to_owned()));
                        }
                        has_text = true;
                        event["content_block"]["text"].as_str()
                    }
                    Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                        event["delta"]["text"].as_str()
                    }
                    _ => None,
                };
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    let _ = output.send(super::Reply::Text(message_id.clone(), text.to_owned()));
                }
            };
            let routed = context.runtime.block_on(async {
                tokio::select! { biased; _ = context.cancelled.cancelled() => Err(ClaudeCliError::Disconnected),
                    result = router.execute_anthropic_passthrough(&context.route,&request,&context.incoming,&headers,&mut observe) => result.map_err(ClaudeCliError::Router) }
            })?;
            metrics
                .response(&routed.body)
                .map_err(ClaudeCliError::Router)?;
            use std::io::Write;
            stream
                .write_all(&crate::http::response::response(
                    &format!(
                        "HTTP/1.1 {} {}",
                        routed.status,
                        crate::http::response::status_text(routed.status)
                    ),
                    &routed.content_type,
                    &routed.body,
                    &[],
                ))
                .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_write_failed"))?;
            if super::super::output_budget::exhausted(&routed.body) {
                return Err(ClaudeCliError::Failure(super::super::output_budget::ERROR));
            }
            continue;
        }
        if path != "/mcp" {
            protocol::reply(
                &mut stream,
                if path == "/api/hello" { 200 } else { 404 },
                &json!({}),
            )?;
            continue;
        }
        let id = request["id"].clone();
        let result = match request["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{},"prompts":{}},"serverInfo":{"name":"emp-codex","version":crate::VERSION}})
            }
            Some("tools/list") => {
                json!({"tools":[{"name":"codex","description":"Ask Codex to execute the available tools and wait for their actual results.","inputSchema":projection::proposal_schema()}]})
            }
            Some("prompts/list") => {
                json!({"prompts":[{"name":"context","description":CONTEXT_PROMPT_DESCRIPTION}]})
            }
            Some("prompts/get") if request["params"]["name"] == "context" => {
                if context.local {
                    output
                        .send(super::Reply::Dispatched)
                        .map_err(|_| ClaudeCliError::Disconnected)?;
                }
                let content = context
                    .context
                    .take()
                    .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_history"))?;
                json!({"messages":content.into_iter().map(|block| json!({"role":"user","content":block})).collect::<Vec<_>>()})
            }
            Some("tools/call") if request["params"]["name"] == "codex" => {
                let arguments = &request["params"]["arguments"];
                if arguments["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
                    && pending.is_none()
                {
                    if context.local {
                        metrics
                            .drain(&context.config, &context.session)
                            .map_err(|_| ClaudeCliError::Failure("claude_cli_invalid_output"))?;
                    }
                    let proposal =
                        json!({"answer":arguments["answer"],"tool_calls":arguments["tool_calls"]});
                    output
                        .send(super::Reply::Complete(metrics.output(proposal), Ok(())))
                        .map_err(|_| ClaudeCliError::Disconnected)?;
                    pending = Some((stream, id));
                    continue;
                } else {
                    json!({"content":[{"type":"text","text":"Provide available Codex tool calls, or write the final answer as normal text."}],"isError":true})
                }
            }
            _ => json!({}),
        };
        protocol::reply(
            &mut stream,
            if id.is_null() { 202 } else { 200 },
            &json!({"jsonrpc":"2.0","id":id,"result":result}),
        )?;
        if request["method"] == "prompts/list" && !prompt_listed {
            terminal.write(b"/emp_codex:context")?;
            prompt_listed = true;
        }
        if context.local && request["method"] == "tools/call" {
            output
                .send(super::Reply::Dispatched)
                .map_err(|_| ClaudeCliError::Disconnected)?;
        }
    }
}
