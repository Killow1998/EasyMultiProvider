//! Reject active host customizations before starting the official CLI.
use super::Error;
use std::path::Path;

fn read_json(path: &Path) -> Result<Option<serde_json::Value>, Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error("agy_cli_home_unavailable")),
    };
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(Error("agy_cli_customizations_present"));
    }
    let bytes = std::fs::read(path).map_err(|_| Error("agy_cli_home_unavailable"))?;
    // The CLI creates empty placeholder files for unconfigured integrations.
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| Error("agy_cli_customizations_present"))
}

fn empty(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(v) => v.is_empty(),
        serde_json::Value::Object(v) => v.values().all(empty),
        _ => false,
    }
}

pub(super) fn validate_home(home: &Path) -> Result<(), Error> {
    for directory in [".gemini/config", ".gemini/antigravity-cli"] {
        let root = home.join(directory);
        for name in [
            "hooks.json",
            "mcp_config.json",
            "skills.json",
            "plugins.json",
        ] {
            if read_json(&root.join(name))?
                .as_ref()
                .is_some_and(|v| !empty(v))
            {
                return Err(Error("agy_cli_customizations_present"));
            }
        }
        for name in ["skills", "plugins", "rules", "agents"] {
            match std::fs::read_dir(root.join(name)) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        return Err(Error("agy_cli_customizations_present"));
                    }
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(Error("agy_cli_home_unavailable"));
                }
                _ => {}
            }
        }
    }
    let root = home.join(".gemini/config");
    // The fixed CLI project must not attach another workspace or custom policy.
    if let Some(project) = read_json(&root.join("projects/default-cli-project.json"))?
        && project.as_object().is_none_or(|p| {
            p.iter()
                .any(|(key, value)| !matches!(key.as_str(), "id" | "name") && !empty(value))
        })
    {
        return Err(Error("agy_cli_customizations_present"));
    }
    if let Some(settings) = read_json(&home.join(".gemini/antigravity-cli/settings.json"))? {
        if !settings.is_object()
            || [
                "hooks",
                "mcpServers",
                "skills",
                "plugins",
                "statusLine",
                "title",
            ]
            .iter()
            .any(|key| settings.get(*key).is_some_and(|v| !empty(v)))
        {
            return Err(Error("agy_cli_customizations_present"));
        }
        if settings
            .get("modelProvider")
            .is_some_and(|v| !v.is_null() && v != "antigravity")
        {
            return Err(Error("agy_cli_login_mode_required"));
        }
    }
    Ok(())
}
