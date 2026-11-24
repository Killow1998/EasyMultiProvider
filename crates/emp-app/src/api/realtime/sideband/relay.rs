use crate::app::ServerState;
use emp_transport::{AsyncClientWebSocket, ClientWebSocketError, WebSocketEvent, split_websocket};
use tokio::sync::mpsc;

pub(crate) const SIDEBAND_EVENT_CAPACITY: usize = 8;
const SIDEBAND_COMMAND_CAPACITY: usize = 1;

enum Command {
    Text(u64, String),
    Close(u16, &'static str),
}

enum UpstreamEvent {
    Text(String),
    Closed(Option<u16>),
    ReadFailed,
    WriteFailed,
}

pub(crate) async fn run(
    state: &ServerState,
    mut upstream: AsyncClientWebSocket,
    socket: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    prefix: &[u8],
    maximum: usize,
) {
    let Ok((mut reader, writer)) = split_websocket(socket, prefix, maximum, None) else {
        upstream.close(1009, "websocket message too large").await;
        return;
    };
    let (commands, command_rx) = mpsc::channel(SIDEBAND_COMMAND_CAPACITY);
    let (event_tx, mut events) = mpsc::channel(SIDEBAND_EVENT_CAPACITY);
    let connection_id = crate::util::random_hex(8).ok();
    let mut command_id = 0u64;

    let downstream = async {
        let (code, reason) = loop {
            tokio::select! {
                _ = state.wait_for_shutdown() => {
                    writer.close(1001, "EMP connection is draining").await;
                    break (1001, "EMP connection is draining");
                }
                _ = state.connection_admission.wait_for_websocket_drain() => {
                    writer.close(1001, "EMP connection is draining").await;
                    break (1001, "EMP connection is draining");
                }
                event = events.recv() => {
                    let (code, reason) = match event {
                        Some(UpstreamEvent::Text(text)) => {
                            if writer.send_text(text.as_bytes()).await.is_ok() {
                                continue;
                            }
                            break (1001, "downstream disconnected");
                        }
                        Some(UpstreamEvent::Closed(code)) => (valid_close_code(code.unwrap_or(1000)), ""),
                        Some(UpstreamEvent::WriteFailed) => (1011, "native sideband write was not confirmed"),
                        Some(UpstreamEvent::ReadFailed) => (1011, "native sideband failed"),
                        None => (1011, "native sideband disconnected"),
                    };
                    writer.close(code, reason).await;
                    break (1000, "");
                }
                incoming = async {
                    let permit = commands.reserve().await?;
                    Ok::<_, mpsc::error::SendError<()>>((permit, reader.read().await))
                }, if !commands.is_closed() => {
                    let Ok((permit, event)) = incoming else {
                        continue;
                    };
                    match event {
                        Ok(WebSocketEvent::Text(text)) => {
                            command_id = command_id.wrapping_add(1);
                            state.backend.diagnostics.journal.event("info", "command_admitted", &serde_json::json!({"connection_id":connection_id, "command_id":command_id, "operation":"sideband_write"}));
                            permit.send(Command::Text(command_id, text));
                        }
                        Ok(WebSocketEvent::Ping(payload)) => {
                            if writer.send_pong(&payload).await.is_err() {
                                break (1011, "downstream websocket failed");
                            }
                        }
                        Ok(WebSocketEvent::Closed { code, payload }) => {
                            writer.reply_close(&payload).await;
                            break (valid_close_code(code.unwrap_or(1000)), "");
                        }
                        Err(error) => {
                            let code = valid_close_code(error.close_code());
                            writer.close(code, &error.to_string()).await;
                            break (code, "downstream websocket failed");
                        }
                    }
                }
            }
        };
        events.close();
        let _ = commands.send(Command::Close(code, reason)).await;
    };
    tokio::join!(
        downstream,
        relay_upstream(upstream, command_rx, event_tx, |id, result| {
            let (level, event, class) = match result {
                Some(Ok(())) => ("info", "command_confirmed", "none"),
                Some(Err(_)) => ("warning", "command_failed", "write_failed"),
                None => ("warning", "command_failed", "worker_stopped"),
            };
            state.backend.diagnostics.journal.event(level, event, &serde_json::json!({"connection_id":connection_id, "command_id":id, "operation":"sideband_write", "error_class":class}));
        }),
    );
}

async fn relay_upstream(
    mut client: AsyncClientWebSocket,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<UpstreamEvent>,
    written: impl Fn(u64, Option<Result<(), ClientWebSocketError>>),
) {
    let mut pending = None;
    let terminal = loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Text(id, text)) => {
                    let result = client.send_text(&text).await;
                    written(id, Some(result));
                    if result.is_err() {
                        client.close(1000, "").await;
                        break Some(UpstreamEvent::WriteFailed);
                    }
                }
                Some(Command::Close(code, reason)) => {
                    client.close(code, reason).await;
                    break None;
                }
                None => {
                    client.close(1000, "").await;
                    break None;
                }
            },
            permit = events.reserve(), if pending.is_some() && !events.is_closed() => {
                if let Ok(permit) = permit {
                    permit.send(pending.take().unwrap());
                }
            }
            event = client.read(), if pending.is_none() => match event {
                Ok(WebSocketEvent::Text(text)) => pending = Some(UpstreamEvent::Text(text)),
                Ok(WebSocketEvent::Ping(payload)) => {
                    if client.send_pong(&payload).await.is_err() {
                        break Some(UpstreamEvent::ReadFailed);
                    }
                }
                Ok(WebSocketEvent::Closed { code, .. }) => {
                    client.close(code.unwrap_or(1000), "").await;
                    break Some(UpstreamEvent::Closed(code));
                }
                Err(_) => {
                    client.close(1000, "").await;
                    break Some(UpstreamEvent::ReadFailed);
                }
            }
        }
    };
    commands.close();
    while let Ok(command) = commands.try_recv() {
        if let Command::Text(id, _) = command {
            written(id, None);
        }
    }
    if let Some(terminal) = terminal {
        let _ = events.send(terminal).await;
    }
}

fn valid_close_code(code: u16) -> u16 {
    if (1000..=1014).contains(&code) && !(1004..=1006).contains(&code)
        || (3000..=4999).contains(&code)
    {
        code
    } else {
        1011
    }
}

#[cfg(test)]
mod tests;
