use super::{Path, Result, read};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};
use regex::Regex;

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("{}: {error}", root.display()))?;
    let scheme = Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]*:").unwrap();
    for path in paths {
        let file = root.join(path);
        if !file.is_file() {
            continue;
        }
        let parent = file.parent().expect("Markdown file has a parent");
        let contents = read(root, path)?;
        let parser = Parser::new_ext(
            &contents,
            Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES,
        );
        for (event, range) in parser.into_offset_iter() {
            let Event::Start(
                Tag::Link {
                    link_type: LinkType::Inline,
                    dest_url,
                    ..
                }
                | Tag::Image {
                    link_type: LinkType::Inline,
                    dest_url,
                    ..
                },
            ) = event
            else {
                continue;
            };
            let target = dest_url.split(['?', '#']).next().unwrap_or_default();
            if target.is_empty() || Path::new(target).has_root() || scheme.is_match(target) {
                continue;
            }
            let valid = decode_relative_path(target)
                .and_then(|decoded| parent.join(decoded).canonicalize().ok())
                .is_some_and(|resolved| {
                    resolved.starts_with(&canonical_root)
                        && (resolved.is_file() || resolved.is_dir())
                });
            if !valid {
                let line = contents[..range.start]
                    .bytes()
                    .filter(|&byte| byte == b'\n')
                    .count()
                    + 1;
                errors.push(format!("[doc-links] {path}:{line}: {target} 不存在"));
            }
        }
    }
    Ok(())
}

fn decode_relative_path(path: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(path.len() + 2);
    decoded.extend_from_slice(b"./");
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        };
        decoded.push(byte);
    }
    String::from_utf8(decoded).ok()
}
