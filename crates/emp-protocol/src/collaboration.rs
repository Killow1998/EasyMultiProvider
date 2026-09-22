//! Python-compatible plaintext collaboration transport for native Responses.
//! Existing ciphertext is never decoded or relabelled as a plaintext task.

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use serde_json::{Map, Value, json};
use std::fmt::{self, Write as _};

const NAMESPACE: &str = "emp_collaboration";
const MESSAGE_TOOLS: &[&str] = &["spawn_agent", "send_message", "followup_task"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollaborationError {
    NamespaceCollision,
    UnexpectedEncryptedArguments,
    TypeError,
    AttributeError,
}

impl CollaborationError {
    pub const fn python_type(self) -> &'static str {
        match self {
            Self::NamespaceCollision => "CollaborationNamespaceCollision",
            Self::UnexpectedEncryptedArguments => "ValueError",
            Self::TypeError => "TypeError",
            Self::AttributeError => "AttributeError",
        }
    }

    pub const fn status(self) -> u16 {
        match self {
            Self::NamespaceCollision => 422,
            // The Python HTTP handler exposes ValueError as a bad request.
            Self::UnexpectedEncryptedArguments => 400,
            _ => 500,
        }
    }

    pub const fn failure_reason(self) -> Option<&'static str> {
        match self {
            Self::NamespaceCollision => Some("collaboration_namespace_collision"),
            _ => None,
        }
    }
}

impl fmt::Display for CollaborationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NamespaceCollision => "reserved EMP collaboration namespace collision",
            Self::UnexpectedEncryptedArguments => "unexpected encrypted collaboration arguments",
            Self::TypeError => "invalid collaboration container type",
            Self::AttributeError => "invalid collaboration object",
        })
    }
}
impl std::error::Error for CollaborationError {}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

// Iterating Python dict/string values yields keys/characters, which cannot be
// tool objects. Preserve those no-ops while retaining non-iterable failures.
fn object_iter(value: &Value) -> Result<Option<&[Value]>, CollaborationError> {
    match value {
        Value::Array(values) => Ok(Some(values)),
        Value::Object(_) | Value::String(_) => Ok(None),
        _ => Err(CollaborationError::TypeError),
    }
}

fn tools(container: &Map<String, Value>) -> Result<&[Value], CollaborationError> {
    match container.get("tools").filter(|value| truthy(value)) {
        Some(value) => Ok(object_iter(value)?.unwrap_or_default()),
        None => Ok(&[]),
    }
}

fn message_tool(name: Option<&Value>) -> Result<bool, CollaborationError> {
    match name {
        Some(Value::Array(_) | Value::Object(_)) => Err(CollaborationError::TypeError),
        value => Ok(value
            .and_then(Value::as_str)
            .is_some_and(|name| MESSAGE_TOOLS.contains(&name))),
    }
}

fn history_containers(body: &Map<String, Value>) -> impl Iterator<Item = &Map<String, Value>> {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .filter_map(Value::as_object)
}

/// Content-free diagnostics. Ignore malformed non-list tools like Python.
pub fn collaboration_summary(body: &Map<String, Value>) -> Value {
    let mut native = 0;
    let mut emp = 0;
    let mut emp_in_history = 0;
    for (container, in_history) in std::iter::once((body, false))
        .chain(history_containers(body).map(|container| (container, true)))
    {
        for tool in container
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match tool.get("name").and_then(Value::as_str) {
                Some("collaboration") => native += 1,
                Some(NAMESPACE) => {
                    emp += 1;
                    emp_in_history += usize::from(in_history);
                }
                _ => {}
            }
        }
    }
    json!({"native": native, "emp": emp, "emp_in_history": emp_in_history})
}

fn project_tools(container: &mut Map<String, Value>) -> Result<bool, CollaborationError> {
    // Validate Python's iteration before mutably borrowing the array.
    tools(container)?;
    let Some(values) = container.get_mut("tools").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let mut changed = false;
    for tool in values {
        if tool.get("type").and_then(Value::as_str) != Some("namespace")
            || tool.get("name").and_then(Value::as_str) != Some("collaboration")
        {
            continue;
        }
        tool["name"] = NAMESPACE.into();
        changed = true;
        let Some(children) = tool.get_mut("tools") else {
            continue;
        };
        let children = match children {
            Value::Array(children) => children,
            Value::Object(entries) if entries.is_empty() => continue,
            Value::String(text) if text.is_empty() => continue,
            Value::Object(_) | Value::String(_) => return Err(CollaborationError::AttributeError),
            _ => return Err(CollaborationError::TypeError),
        };
        for child in children {
            let child = child
                .as_object_mut()
                .ok_or(CollaborationError::AttributeError)?;
            if !message_tool(child.get("name"))? {
                continue;
            }
            let Some(parameters) = child.get_mut("parameters") else {
                continue;
            };
            let parameters = parameters
                .as_object_mut()
                .ok_or(CollaborationError::AttributeError)?;
            let Some(properties) = parameters.get_mut("properties") else {
                continue;
            };
            let properties = properties
                .as_object_mut()
                .ok_or(CollaborationError::AttributeError)?;
            if let Some(message) = properties.get_mut("message").and_then(Value::as_object_mut) {
                message.remove("encrypted");
            }
        }
    }
    Ok(changed)
}

/// Change only plaintext-capable tool transport metadata. Generated additional
/// tool IDs are deterministic UUID5 values and must match Python exactly.
pub fn prepare_collaboration(
    body: &Map<String, Value>,
) -> Result<(Value, bool), CollaborationError> {
    // Collision detection covers every tool container before any projection.
    let mut collision = false;
    for container in std::iter::once(body).chain(history_containers(body)) {
        collision |= tools(container)?
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(NAMESPACE));
    }
    if collision {
        return Err(CollaborationError::NamespaceCollision);
    }
    let mut result = body.clone();
    let mut changed = project_tools(&mut result)?;
    if let Some(input) = result.get_mut("input").and_then(Value::as_array_mut) {
        for item in input
            .iter_mut()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        {
            changed |= project_tools(item.as_object_mut().expect("additional tools object"))?;
        }
        if changed {
            for item in input {
                if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                    let container = item.as_object_mut().expect("additional tools object");
                    if let Some(id) = container.get("id")
                        && tools(container)?
                            .iter()
                            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(NAMESPACE))
                    {
                        let name = python_str(id) + &python_json(&container["tools"]);
                        container.insert("id".to_owned(), Value::String(tool_schema_id(&name)));
                    }
                } else if item.get("type").and_then(Value::as_str) == Some("function_call")
                    && item.get("namespace").and_then(Value::as_str) == Some("collaboration")
                    && item
                        .get("encrypted_function_args")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
                {
                    item["namespace"] = NAMESPACE.into();
                }
            }
        }
    }
    Ok((Value::Object(result), changed))
}

fn restore_item(item: &mut Value) -> Result<(), CollaborationError> {
    if item.get("type").and_then(Value::as_str) == Some("function_call")
        && item.get("namespace").and_then(Value::as_str) == Some(NAMESPACE)
    {
        item["namespace"] = "collaboration".into();
        if message_tool(item.get("name"))? {
            if item.get("encrypted_function_args").is_some_and(truthy) {
                return Err(CollaborationError::UnexpectedEncryptedArguments);
            }
            item["encrypted_function_args"] = json!([]);
        }
    }
    Ok(())
}

fn restore_output(container: &mut Value) -> Result<(), CollaborationError> {
    if let Some(output) = container
        .as_object_mut()
        .and_then(|object| object.get_mut("output"))
    {
        object_iter(output)?;
        if let Some(items) = output.as_array_mut() {
            for item in items {
                restore_item(item)?;
            }
        }
    }
    Ok(())
}

/// Restore namespace/explicit plaintext markers only on collaboration tool items.
pub fn restore_collaboration(value: &Value) -> Result<Value, CollaborationError> {
    if !value.is_object() {
        return Ok(value.clone());
    }
    if matches!(value.get("type"), Some(Value::Array(_) | Value::Object(_))) {
        return Err(CollaborationError::TypeError);
    }
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "function_call"
                | "response.output_item.added"
                | "response.output_item.done"
                | "response.completed"
                | "response.failed"
                | "response.incomplete"
        )
    ) && value.get("output").is_none()
    {
        return Ok(value.clone());
    }
    let mut result = value.clone();
    restore_item(&mut result)?;
    if let Some(item) = result.get_mut("item") {
        restore_item(item)?;
    }
    restore_output(&mut result)?;
    if let Some(response) = result.get_mut("response") {
        restore_output(response)?;
    }
    Ok(result)
}

fn tool_schema_id(name: &str) -> String {
    // UUID5 uses SHA-1 for identity, not security or credential hashing.
    let namespace_url = [
        0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30,
        0xc8,
    ];
    let mut context = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    context.update(&namespace_url);
    context.update(name.as_bytes());
    let digest = context.finish();
    let mut id = digest.as_ref()[..16].to_vec();
    id[6] = (id[6] & 0x0f) | 0x50;
    id[8] = (id[8] & 0x3f) | 0x80;
    let mut result = String::from("at_");
    for byte in id {
        write!(result, "{byte:02x}").expect("write to String");
    }
    result
}

fn python_number(number: &serde_json::Number) -> String {
    let rendered = number.to_string();
    if !number.is_f64() {
        return rendered;
    }
    if let Some((mantissa, exponent)) = rendered.split_once('e')
        && let Ok(exponent) = exponent.parse::<i32>()
    {
        return format!("{mantissa}e{exponent:+03}");
    }
    let (sign, unsigned) = rendered
        .strip_prefix('-')
        .map_or(("", rendered.as_str()), |v| ("-", v));
    if let Some(fraction) = unsigned.strip_prefix("0.")
        && let Some(index) = fraction.bytes().position(|byte| byte != b'0')
        && index >= 4
    {
        let digits = &fraction[index..];
        let mantissa = if digits.len() == 1 {
            digits.to_owned()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        return format!("{sign}{mantissa}e-{:02}", index + 1);
    }
    rendered
}

// JSON schema IDs depend on Python's default spaces and ensure_ascii=True,
// including surrogate pairs. Do not replace this with compact serde JSON.
fn python_json(value: &Value) -> String {
    match value {
        Value::Number(number) => python_number(number),
        Value::String(text) => {
            let encoded = serde_json::to_string(text).expect("JSON string");
            let mut result = String::new();
            for character in encoded.chars() {
                if character < '\u{7f}' {
                    result.push(character);
                } else {
                    for unit in character.encode_utf16(&mut [0; 2]).iter() {
                        write!(result, "\\u{unit:04x}").expect("write to String");
                    }
                }
            }
            result
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => {
            let mut items = items.iter().collect::<Vec<_>>();
            items.sort_by(|(a, _), (b, _)| a.cmp(b));
            format!(
                "{{{}}}",
                items
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}: {}",
                        python_json(&Value::String(key.clone())),
                        python_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => value.to_string(),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => {
            let quote = if text.contains('\'') && !text.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut result = String::from(quote);
            for c in text.chars() {
                match c {
                    '\\' => result.push_str("\\\\"),
                    '\n' => result.push_str("\\n"),
                    '\r' => result.push_str("\\r"),
                    '\t' => result.push_str("\\t"),
                    c if c == quote => {
                        result.push('\\');
                        result.push(c);
                    }
                    c if c.is_control() || (c.is_whitespace() && c != ' ') => {
                        if u32::from(c) <= 0xff {
                            write!(result, "\\x{:02x}", u32::from(c)).unwrap();
                        } else if u32::from(c) <= 0xffff {
                            write!(result, "\\u{:04x}", u32::from(c)).unwrap();
                        } else {
                            write!(result, "\\U{:08x}", u32::from(c)).unwrap();
                        }
                    }
                    c => result.push(c),
                }
            }
            result.push(quote);
            result
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    python_repr(&Value::String(k.clone())),
                    python_repr(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => python_str(value),
    }
}

pub(crate) fn python_str(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(value) => if *value { "True" } else { "False" }.to_owned(),
        Value::Number(value) => python_number(value),
        _ => python_repr(value),
    }
}
