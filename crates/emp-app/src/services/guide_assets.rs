use crate::server::http::error::HttpReply;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use std::collections::BTreeMap;
use url::Url;

pub(crate) fn asset_reply(path: &str) -> Option<HttpReply> {
    let path = if let Some(gif) = path.strip_prefix("/assets/tutorial-") {
        format!("docs/assets/tutorials/{gif}")
    } else {
        path.strip_prefix("/guide/")?.to_owned()
    };
    let reply = match Resource::find(&path)? {
        Resource::Markdown(bytes) => render_doc(&path, bytes),
        Resource::Gif(bytes) => HttpReply::new(200, bytes).with_content_type("image/gif"),
    };
    Some(reply.with_header("Cache-Control", "no-store"))
}

enum Resource {
    Markdown(&'static [u8]),
    Gif(&'static [u8]),
}

impl Resource {
    fn find(path: &str) -> Option<Self> {
        macro_rules! embedded {
            ($kind:ident, $path:literal) => {
                Self::$kind(include_bytes!(concat!("../../../../", $path)))
            };
        }

        Some(match path {
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
}

fn render_doc(path: &str, bytes: &[u8]) -> HttpReply {
    let base = Url::parse(&format!(
        "https://github.com/Killow1998/EasyMultiProvider/blob/main/{path}"
    ))
    .expect("known guide URL");
    let markdown = std::str::from_utf8(bytes).expect("embedded guide Markdown");
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
    HttpReply::new(200, page.into_bytes()).with_content_type("text/html; charset=utf-8")
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, response::IntoResponse};

    #[tokio::test]
    async fn embedded_installation_guide_renders_the_linux_anchor() {
        let reply = asset_reply("/guide/docs/self-update.md").expect("embedded installation guide");
        let response = reply.into_response();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let page = std::str::from_utf8(&bytes).unwrap();
        assert!(page.contains(
            r#"<h2 id="linux-installation-and-migration">Linux installation and migration</h2>"#
        ));
    }

    #[tokio::test]
    async fn markdown_filters_raw_html_and_unsafe_links_and_rewrites_relative_links() {
        let response = render_doc(
            "README.md",
            b"# Offline guide\n\n<script>alert('ignored')</script>\n\n\
              <em>Inline HTML</em> [Unsafe](javascript:alert%281%29) | [Guide](docs/tutorials.md)\n",
        )
        .into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let page = std::str::from_utf8(&bytes).unwrap();
        assert!(page.contains(r#"<h1 id="offline-guide">Offline guide</h1>"#));
        assert!(!page.contains("<script>"));
        assert!(!page.contains("<em>"));
        assert!(!page.contains("javascript:"));
        assert!(page.contains(r#"href="about:blank">Unsafe</a>"#));
        assert!(page.contains(r#"href="/guide/docs/tutorials.md">Guide</a>"#));
    }
}
