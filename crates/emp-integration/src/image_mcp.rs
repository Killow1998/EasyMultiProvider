//! Lease the optional image MCP entry without changing the model server.
use crate::{FieldState, IntegrationError};
use toml_edit::{DocumentMut, Item, Table};

pub(super) fn state(document: &DocumentMut) -> Result<FieldState, IntegrationError> {
    let Some(server) = document
        .get("mcp_servers")
        .and_then(|servers| servers.get("emp_images"))
    else {
        return Ok(FieldState::absent());
    };
    if !server.is_table_like() {
        return Err(IntegrationError("EMP image MCP entry is not a table"));
    }
    let mut encoded = DocumentMut::new();
    encoded["emp_images"] = server.clone();
    Ok(FieldState::value(encoded.to_string()))
}

pub(super) fn definition(base_url: &str, settings: &crate::ImageMcpConfig) -> FieldState {
    let mut document = DocumentMut::new();
    let mut server = Table::new();
    server["command"] = toml_edit::value(&settings.command);
    let mut args = toml_edit::Array::new();
    for value in ["--emp-image-mcp", base_url, &settings.key_file] {
        args.push(value);
    }
    server["args"] = toml_edit::value(args);
    server["tool_timeout_sec"] = toml_edit::value(300);
    document["emp_images"] = Item::Table(server);
    FieldState::value(document.to_string())
}

pub(super) fn set(document: &mut DocumentMut, state: &FieldState) -> Result<(), IntegrationError> {
    if !state.present {
        if let Some(servers) = document.get_mut("mcp_servers") {
            servers
                .as_table_like_mut()
                .ok_or(IntegrationError("MCP servers is not a table"))?
                .remove("emp_images");
            if servers
                .as_table()
                .is_some_and(|table| table.is_implicit() && table.is_empty())
            {
                document.remove("mcp_servers");
            }
        }
        return Ok(());
    }
    let mut encoded = state
        .value
        .as_deref()
        .unwrap_or_default()
        .parse::<DocumentMut>()
        .map_err(|_| IntegrationError("invalid EMP image MCP lease"))?;
    let mut server = encoded
        .remove("emp_images")
        .filter(Item::is_table_like)
        .ok_or(IntegrationError("invalid EMP image MCP lease"))?;
    let servers = document.entry("mcp_servers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    if servers.is_inline_table()
        && let Item::Table(table) = server
    {
        server = toml_edit::value(table.into_inline_table());
    }
    servers
        .as_table_like_mut()
        .ok_or(IntegrationError("MCP servers is not a table"))?
        .insert("emp_images", server);
    Ok(())
}
