use super::{Path, Result, read};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};
use regex::Regex;

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    let scheme = Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]*:").unwrap();
    for path in paths {
        let file = root.join(path);
        if !file.is_file() {
            continue;
        }
        let parent = file.parent().expect("Markdown file has a parent");
        let contents = read(root, path)?;
        let parser = Parser::new_ext(&contents, Options::ENABLE_TABLES);
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
            let target = dest_url.split('#').next().unwrap_or_default();
            if target.is_empty() || Path::new(target).has_root() || scheme.is_match(target) {
                continue;
            }
            let resolved = parent.join(target);
            if !resolved.is_file() && !resolved.is_dir() {
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
