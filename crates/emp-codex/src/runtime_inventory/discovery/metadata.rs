//! Optional host metadata; never infer an engine version from a bundle name.
use std::io::Read;
use std::path::Path;

fn bounded_bytes(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 256 * 1024 {
        return None;
    }
    Some(bytes)
}
fn version(value: &str) -> Option<String> {
    (value.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && value.contains('.')
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')))
    .then(|| value.to_owned())
}

pub(super) fn host_version(source: &str, binary: &Path) -> Option<String> {
    match source {
        "codex_app" => {
            // The outer host, not its nested CodexCLI engine bundle.
            let bundle = binary
                .ancestors()
                .filter(|p| p.extension().is_some_and(|s| s == "app"))
                .last()?;
            let bytes = bounded_bytes(&bundle.join("Contents/Info.plist"))?;
            let document = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
            version(
                document
                    .as_dictionary()?
                    .get("CFBundleShortVersionString")?
                    .as_string()?,
            )
        }
        "vscode" | "vscode_insiders" | "cursor" => {
            let extension = binary.parent()?.parent()?.parent()?;
            let document: serde_json::Value =
                serde_json::from_slice(&bounded_bytes(&extension.join("package.json"))?).ok()?;
            version(document.get("version")?.as_str()?)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outer_app_and_editor_metadata_are_distinct_from_engine() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("ChatGPT.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), "<plist version=\"1.0\"><dict><key>CFBundleShortVersionString</key><string>26.924.22138</string></dict></plist>").unwrap();
        let engine = bundle.join("Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex");
        assert_eq!(
            host_version("codex_app", &engine).as_deref(),
            Some("26.924.22138")
        );
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert(
            "CFBundleShortVersionString".into(),
            plist::Value::String("26.924.22138".into()),
        );
        plist::Value::Dictionary(dictionary)
            .to_file_binary(bundle.join("Contents/Info.plist"))
            .unwrap();
        assert_eq!(
            host_version("codex_app", &engine).as_deref(),
            Some("26.924.22138")
        );
        let extension = dir.path().join("openai.chatgpt-incorrect-folder-version");
        std::fs::create_dir(&extension).unwrap();
        std::fs::write(extension.join("package.json"), r#"{"version":"0.5.17"}"#).unwrap();
        assert_eq!(
            host_version("vscode", &extension.join("bin/macos-x86_64/codex")).as_deref(),
            Some("0.5.17")
        );
        assert_eq!(host_version("path_cli", &engine), None);
    }
}
