//! Request-owned tool identities, allowed choices and unambiguous spellings.
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ToolIdentity {
    pub(super) namespace: String,
    pub(super) name: String,
    kind: ToolKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ToolKind {
    Function,
    Custom,
    Search,
}

pub(super) fn available_tools(request: &Value) -> BTreeSet<ToolIdentity> {
    let mut tools = collect_tools(request);
    tools.retain(|tool| tool_choice_allows(request.get("tool_choice"), tool));
    tools
}

pub(super) fn resolve_tool<'a>(
    available: &'a BTreeSet<ToolIdentity>,
    namespace: &str,
    name: &str,
    kind: ToolKind,
) -> Option<&'a ToolIdentity> {
    let exact = ToolIdentity {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        kind,
    };
    if let Some(tool) = available.get(&exact) {
        return Some(tool);
    }
    // Restore only an unambiguous spelling of an already available identity.
    // Never invent a tool, cross kinds or override an explicitly wrong scope.
    let mut matches = available.iter().filter(|tool| {
        tool.kind == kind
            && (namespace.is_empty() || namespace == tool.namespace)
            && (name == tool.name
                || (!tool.namespace.is_empty()
                    && [
                        format!("{}.{}", tool.namespace, tool.name),
                        format!("{}__{}", tool.namespace, tool.name),
                    ]
                    .iter()
                    .any(|qualified| qualified == name)))
    });
    let tool = matches.next()?;
    matches.next().is_none().then_some(tool)
}

fn collect_tools(body: &Value) -> BTreeSet<ToolIdentity> {
    let mut tools = BTreeSet::new();
    collect_tool_definitions(body.get("tools"), "", &mut tools);
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            if matches!(
                item.get("type").and_then(Value::as_str),
                Some("additional_tools" | "tool_search_output")
            ) {
                collect_tool_definitions(item.get("tools"), "", &mut tools);
            }
        }
    }
    tools
}

fn tool_choice_allows(choice: Option<&Value>, tool: &ToolIdentity) -> bool {
    match choice {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value != "none",
        Some(Value::Object(choice)) => match choice.get("type").and_then(Value::as_str) {
            Some("allowed_tools") => {
                choice
                    .get("tools")
                    .and_then(Value::as_array)
                    .is_some_and(|tools| {
                        tools.iter().any(|allowed| {
                            let name = allowed.get("name").and_then(Value::as_str);
                            let namespace = allowed
                                .get("namespace")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            name == Some(tool.name.as_str())
                                && namespace == tool.namespace
                                && match allowed.get("type").and_then(Value::as_str) {
                                    Some("custom") => tool.kind == ToolKind::Custom,
                                    Some("tool_search") => tool.kind == ToolKind::Search,
                                    _ => tool.kind == ToolKind::Function,
                                }
                        })
                    })
            }
            Some("function" | "custom") => {
                choice.get("name").and_then(Value::as_str) == Some(tool.name.as_str())
                    && choice
                        .get("namespace")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        == tool.namespace
                    && if choice["type"] == "custom" {
                        tool.kind == ToolKind::Custom
                    } else {
                        tool.kind == ToolKind::Function
                    }
            }
            Some("tool_search") => tool.kind == ToolKind::Search,
            _ => true,
        },
        Some(_) => false,
    }
}

fn collect_tool_definitions(
    value: Option<&Value>,
    namespace: &str,
    tools: &mut BTreeSet<ToolIdentity>,
) {
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("namespace") => {
                if let Some(group) = item.get("name").and_then(Value::as_str) {
                    collect_tool_definitions(item.get("tools"), group, tools);
                }
            }
            Some("function" | "custom") => {
                let definition = item
                    .get("function")
                    .filter(|value| value.is_object())
                    .unwrap_or(item);
                if let Some(name) = definition.get("name").and_then(Value::as_str) {
                    tools.insert(ToolIdentity {
                        namespace: namespace.to_owned(),
                        name: name.to_owned(),
                        kind: if item["type"] == "custom" {
                            ToolKind::Custom
                        } else {
                            ToolKind::Function
                        },
                    });
                }
            }
            Some("tool_search")
                if item.get("execution").and_then(Value::as_str) == Some("client") =>
            {
                tools.insert(ToolIdentity {
                    namespace: namespace.to_owned(),
                    name: "tool_search".to_owned(),
                    kind: ToolKind::Search,
                });
            }
            _ => {}
        }
    }
}
