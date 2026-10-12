//! User-facing error summaries belong to delivery, not upstream execution or accounting.
use crate::app::ServerState;
use emp_core::{Dialect, ResolvedRoute};
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct ErrorPresentation {
    chinese: bool,
    service: &'static str,
    selected: bool,
    thread: Option<String>,
}
impl Default for ErrorPresentation {
    fn default() -> Self {
        Self {
            chinese: false,
            service: "Upstream service",
            selected: false,
            thread: None,
        }
    }
}
impl ErrorPresentation {
    pub(crate) fn new(state: &ServerState) -> Self {
        Self {
            chinese: state
                .backend
                .configuration
                .read()
                .ok()
                .is_none_or(|config| config["language"] != "en"),
            ..Self::default()
        }
    }
    pub(crate) fn thread(&mut self, id: Option<&str>) {
        self.thread = id
            .filter(|id| {
                id.len() == 36
                    && id.bytes().enumerate().all(|(i, c)| {
                        if [8, 13, 18, 23].contains(&i) {
                            c == b'-'
                        } else {
                            c.is_ascii_hexdigit()
                        }
                    })
                    && id.as_bytes().iter().filter(|c| **c == b'-').count() == 4
            })
            .map(str::to_owned);
    }
    pub(crate) fn select(&mut self, route: &ResolvedRoute) {
        self.selected = true;
        self.service = if route.dialect == Dialect::CodexNative {
            "Codex"
        } else if route
            .provider
            .value()
            .get("execution_backend")
            .and_then(Value::as_str)
            == Some("claude_cli")
        {
            "Claude Code"
        } else {
            "Upstream service"
        };
    }
    pub(crate) fn event(&self, event: &Value, upstream: bool) -> Option<Value> {
        if !matches!(event["type"].as_str(), Some("error" | "response.failed")) {
            return None;
        }
        let mut candidate = event.clone();
        let status = event["status"]
            .as_u64()
            .or_else(|| event["error"]["status"].as_u64())
            .or_else(|| event["response"]["error"]["status"].as_u64())
            .unwrap_or(0) as u16;
        let detail = if candidate["type"] == "response.failed" {
            candidate.pointer_mut("/response/error")?
        } else {
            candidate.get_mut("error")?
        };
        self.detail(detail, status, upstream)?;
        Some(candidate)
    }
    fn detail(&self, detail: &mut Value, status: u16, upstream: bool) -> Option<()> {
        let original = detail["message"].as_str()?.to_owned();
        let origin = if upstream {
            "upstream"
        } else {
            detail["origin"]
                .as_str()
                .unwrap_or(if self.selected { "upstream" } else { "emp" })
        };
        let code = detail["code"]
            .as_str()
            .or_else(|| detail["error_class"].as_str())
            .unwrap_or("");
        let (source, zh, en) = match origin {
            "transport" => ("EMP", "网络连接失败", "network connection failed"),
            "emp"
                if code == "history_reconstruction_failed"
                    || detail["error_class"] == "history_reconstruction_failed" =>
            {
                ("EMP", "对话历史处理失败", "conversation history failed")
            }
            "emp" if code == "context_length_exceeded" => {
                ("EMP", "请求上下文超限", "request context limit exceeded")
            }
            "emp" => ("EMP", "请求处理出错", "request processing failed"),
            "client" => ("Client", "调用已中断", "request interrupted"),
            _ => {
                let source = if origin == "claude_cli" {
                    "Claude Code"
                } else {
                    self.service
                };
                let reason = detail["failure_reason"].as_str().unwrap_or(code);
                let (zh, en) = match reason {
                    "model_overloaded" | "overloaded_error" | "server_overloaded"
                    | "upstream_capacity" => ("模型拥堵", "model overloaded"),
                    "context_length_exceeded" => ("模型上下文超限", "model context limit exceeded"),
                    _ if status == 429 || matches!(code, "rate_limit_exceeded" | "rate_limit") => {
                        ("请求受限", "rate limit reached")
                    }
                    _ if matches!(status, 401 | 403) => ("登录或授权失败", "authentication failed"),
                    _ if matches!(status, 408 | 504) => ("请求超时", "request timed out"),
                    _ if status == 503 => ("服务暂时不可用", "service temporarily unavailable"),
                    _ => ("服务出错", "service failed"),
                };
                (source, zh, en)
            }
        };
        let label = if self.chinese && source == "Upstream service" {
            "上游服务"
        } else {
            source
        };
        let history = detail["category"] == "checkpoint"
            || matches!(
                code,
                "invalid_encrypted_content" | "invalid_encrypted_content_error"
            )
            || detail["reason"]
                .as_str()
                .is_some_and(|s| s.starts_with("portable_checkpoint_"));
        let summary = format!("{label} {}", if self.chinese { zh } else { en });
        detail["summary"] = summary.clone().into();
        detail["message"] = format!("{summary}\n{original}").into();
        if history && let Some(thread) = &self.thread {
            let command = repair_command(thread);
            detail["repair_command"] = command.clone().into();
            let instruction = if self.chinese {
                "关闭此对话后运行以下命令，再重新打开对话："
            } else {
                "Close this conversation, run the following command, then reopen it:"
            };
            detail["message"] =
                format!("{}\n{instruction}\n{command}", detail["message"].as_str()?).into();
        }
        Some(())
    }
    pub(crate) fn http(&self, wire: &[u8]) -> Option<Vec<u8>> {
        let split = wire.windows(4).position(|part| part == b"\r\n\r\n")?;
        let header = std::str::from_utf8(&wire[..split]).ok()?;
        let status = header.split_whitespace().nth(1)?.parse::<u16>().ok()?;
        if !(400..600).contains(&status) {
            return None;
        }
        let mut body: Value = serde_json::from_slice(&wire[split + 4..]).ok()?;
        self.detail(body.get_mut("error")?, status, false)?;
        let body = serde_json::to_vec(&body).ok()?;
        let mut output = header
            .lines()
            .filter(|line| !line.to_ascii_lowercase().starts_with("content-length:"))
            .collect::<Vec<_>>()
            .join("\r\n")
            .into_bytes();
        output.extend_from_slice(format!("\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes());
        output.extend(body);
        Some(output)
    }
    pub(crate) fn frame(&self, frame: &[u8], upstream: bool) -> Option<Vec<u8>> {
        let text = std::str::from_utf8(frame).ok()?;
        let data = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        let event: Value = serde_json::from_str(&data).ok()?;
        let candidate = self.event(&event, upstream)?;
        let data = serde_json::to_string(&candidate).ok()?;
        let mut lines = text
            .lines()
            .filter(|line| !line.starts_with("data:") && !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !lines.is_empty() {
            lines.push('\n');
        }
        lines.push_str(&format!("data: {data}\n\n"));
        Some(lines.into_bytes())
    }
}

fn repair_command(thread: &str) -> String {
    let executable = std::env::current_exe().ok().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("EMP") || name.eq_ignore_ascii_case("EMP.exe")
            })
    });
    let program = executable
        .as_ref()
        .and_then(|path| path.to_str())
        .map(|path| {
            if cfg!(windows) {
                format!("& '{}'", path.replace('\'', "''"))
            } else {
                format!("'{}'", path.replace('\'', "'\\''"))
            }
        })
        .unwrap_or_else(|| "EMP".into());
    format!("{program} migrate-history --thread-id {thread}")
}

#[cfg(test)]
mod tests;
