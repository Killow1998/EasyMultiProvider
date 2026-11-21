use crate::http::response::response;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

pub(super) const PACK: &str = "tutorials.json";

pub(super) enum Resource {
    Pack,
    Markdown(&'static [u8]),
    Gif(&'static [u8]),
}

impl Resource {
    pub(super) fn find(path: &str) -> Option<Self> {
        macro_rules! embedded {
            ($kind:ident, $path:literal) => {
                Self::$kind(include_bytes!(concat!("../../../../../", $path)))
            };
        }

        Some(match path {
            PACK => Self::Pack,
            "README.md" => embedded!(Markdown, "README.md"),
            "README.zh-CN.md" => embedded!(Markdown, "README.zh-CN.md"),
            "docs/tutorials.md" => embedded!(Markdown, "docs/tutorials.md"),
            "docs/self-update.md" => embedded!(Markdown, "docs/self-update.md"),
            "docs/assets/tutorials/00-native-context.gif" => {
                embedded!(Gif, "docs/assets/tutorials/00-native-context.gif")
            }
            "docs/assets/tutorials/00-native-name.gif" => {
                embedded!(Gif, "docs/assets/tutorials/00-native-name.gif")
            }
            "docs/assets/tutorials/00-remove-subscription.gif" => {
                embedded!(Gif, "docs/assets/tutorials/00-remove-subscription.gif")
            }
            "docs/assets/tutorials/01-add-service.gif" => {
                embedded!(Gif, "docs/assets/tutorials/01-add-service.gif")
            }
            "docs/assets/tutorials/02-import-service.gif" => {
                embedded!(Gif, "docs/assets/tutorials/02-import-service.gif")
            }
            "docs/assets/tutorials/03-export-service.gif" => {
                embedded!(Gif, "docs/assets/tutorials/03-export-service.gif")
            }
            "docs/assets/tutorials/04-remove-service.gif" => {
                embedded!(Gif, "docs/assets/tutorials/04-remove-service.gif")
            }
            "docs/assets/tutorials/05-change-context.gif" => {
                embedded!(Gif, "docs/assets/tutorials/05-change-context.gif")
            }
            "docs/assets/tutorials/06-rename.gif" => {
                embedded!(Gif, "docs/assets/tutorials/06-rename.gif")
            }
            "docs/assets/tutorials/07-context-label.gif" => {
                embedded!(Gif, "docs/assets/tutorials/07-context-label.gif")
            }
            "docs/assets/tutorials/08-tunnel-migration.gif" => {
                embedded!(Gif, "docs/assets/tutorials/08-tunnel-migration.gif")
            }
            "docs/assets/tutorials/09-team-workspace.gif" => {
                embedded!(Gif, "docs/assets/tutorials/09-team-workspace.gif")
            }
            "docs/assets/tutorials/10-team-share.gif" => {
                embedded!(Gif, "docs/assets/tutorials/10-team-share.gif")
            }
            "docs/assets/tutorials/11-team-approve.gif" => {
                embedded!(Gif, "docs/assets/tutorials/11-team-approve.gif")
            }
            "docs/assets/tutorials/12-team-members.gif" => {
                embedded!(Gif, "docs/assets/tutorials/12-team-members.gif")
            }
            _ => return None,
        })
    }

    pub(super) fn embedded(&self) -> Option<&'static [u8]> {
        match self {
            Self::Pack => None,
            Self::Markdown(bytes) | Self::Gif(bytes) => Some(bytes),
        }
    }

    pub(super) fn limit(&self) -> usize {
        match self {
            Self::Pack => 32 * 1024,
            Self::Markdown(_) => 512 * 1024,
            Self::Gif(_) => 8 * 1024 * 1024,
        }
    }

    pub(super) fn valid(&self, bytes: &[u8]) -> bool {
        match self {
            Self::Pack => parse_pack(bytes).is_some(),
            Self::Markdown(_) => std::str::from_utf8(bytes).is_ok(),
            Self::Gif(_) => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TutorialPack {
    schema_version: u8,
    team_join: Localized<JoinLesson>,
    titles: Localized<BTreeMap<String, String>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Localized<T> {
    zh: T,
    en: T,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JoinLesson {
    title: String,
    steps: [String; 3],
}

pub(super) fn parse_pack(bytes: &[u8]) -> Option<TutorialPack> {
    let pack: TutorialPack = serde_json::from_slice(bytes).ok()?;
    (pack.schema_version == 1).then_some(pack)
}

pub(super) fn render_doc(path: &str, bytes: &[u8]) -> Vec<u8> {
    let base = Url::parse(&format!(
        "https://github.com/Killow1998/EasyMultiProvider/blob/main/{path}"
    ))
    .expect("known guide URL");
    let markdown = std::str::from_utf8(bytes).expect("validated guide Markdown");
    let mut events: Vec<_> = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .filter_map(|mut event| {
        match &mut event {
            Event::Html(_) | Event::InlineHtml(_) => return None,
            Event::Start(Tag::Link { dest_url, .. }) => {
                *dest_url = doc_link(&base, dest_url, false).into();
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                *dest_url = doc_link(&base, dest_url, true).into();
            }
            _ => {}
        }
        Some(event)
    })
    .collect();
    let mut heading = None;
    let mut seen = BTreeMap::new();
    let mut has_title = false;
    for index in 0..events.len() {
        match &events[index] {
            Event::Start(Tag::Heading { level, .. }) => {
                has_title |= *level == HeadingLevel::H1;
                heading = Some((index, String::new()));
            }
            Event::Text(part) | Event::Code(part) => {
                if let Some((_, title)) = &mut heading {
                    title.push_str(part);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                let (start, title) = heading.take().expect("paired Markdown heading");
                let slug: String = title
                    .trim()
                    .to_lowercase()
                    .chars()
                    .filter_map(|character| {
                        if character.is_alphanumeric() || matches!(character, '-' | '_') {
                            Some(character)
                        } else {
                            character.is_whitespace().then_some('-')
                        }
                    })
                    .collect();
                let count = seen.entry(slug.clone()).or_insert(0);
                if let Event::Start(Tag::Heading { id, .. }) = &mut events[start] {
                    *id = Some(
                        if *count == 0 {
                            slug
                        } else {
                            format!("{slug}-{count}")
                        }
                        .into(),
                    );
                }
                *count += 1;
            }
            _ => {}
        }
    }
    let mut page = format!(
        r#"<!doctype html><html lang="{}"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="referrer" content="no-referrer"><title>EMP · {}</title><link rel="stylesheet" href="/assets/guide.css"><main>"#,
        if path == "README.zh-CN.md" {
            "zh-CN"
        } else {
            "en"
        },
        path.rsplit('/').next().expect("known guide filename"),
    );
    if path.starts_with("README.") && !has_title {
        page.push_str("<h1>EMP — EasyMultiProvider</h1>");
    }
    html::push_html(&mut page, events.into_iter());
    page.push_str("</main></html>");
    response(
        "HTTP/1.1 200 OK",
        "text/html; charset=utf-8",
        page.as_bytes(),
        &[("Cache-Control", "no-store")],
    )
}

fn doc_link(base: &Url, target: &str, image: bool) -> String {
    if !image && target.starts_with('#') {
        return target.into();
    }
    if let Ok(url) = base.join(target)
        && matches!(url.scheme(), "https" | "http")
    {
        if url.origin() == base.origin()
            && let Some(path) = url
                .path()
                .strip_prefix("/Killow1998/EasyMultiProvider/blob/main/")
            && let Some(resource) = Resource::find(path)
        {
            return match resource {
                Resource::Gif(_) => format!(
                    "/assets/tutorial-{}",
                    path.rsplit('/').next().expect("known GIF")
                ),
                Resource::Markdown(_) if !image => format!(
                    "/guide/{path}{}",
                    url.fragment()
                        .map_or_else(String::new, |fragment| format!("#{fragment}")),
                ),
                _ => "about:blank".to_string(),
            };
        }
        if !image {
            return url.into();
        }
    }
    "about:blank".to_string()
}
