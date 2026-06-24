use scraper::{Html, Selector};
use url::Url;

use crate::canonical::canonicalize_url;
use crate::types::{LinkContext, ParsedOutboundLink};

pub use crate::types::LinkContext as HtmlLinkContext;

fn resolve_href(href: &str, page_url: &str) -> Option<String> {
    if href.starts_with("javascript:") || href.starts_with("data:") {
        return None;
    }
    let base = Url::parse(page_url).ok()?;
    let resolved = base.join(href).ok()?;
    canonicalize_url(resolved.as_str()).map(|c| c.canonical)
}

fn context_from_ancestors(anchor: scraper::element_ref::ElementRef<'_>) -> LinkContext {
    let mut current = anchor.parent();
    while let Some(node) = current {
        if let Some(el) = scraper::ElementRef::wrap(node) {
            match el.value().name() {
                "nav" => return LinkContext::Nav,
                "footer" => return LinkContext::Footer,
                "aside" => return LinkContext::Sidebar,
                _ => {
                    if let Some(class) = el.value().attr("class") {
                        let c = class.to_ascii_lowercase();
                        if c.contains("nav") || c.contains("menu") {
                            return LinkContext::Nav;
                        }
                        if c.contains("footer") {
                            return LinkContext::Footer;
                        }
                        if c.contains("sidebar") {
                            return LinkContext::Sidebar;
                        }
                    }
                }
            }
            current = el.parent();
        } else {
            break;
        }
    }
    LinkContext::Body
}

/// Parse outbound HTTP(S) links from HTML body.
pub fn parse_html_links(
    page_url: &str,
    html: &str,
    max_links: u32,
) -> (Vec<ParsedOutboundLink>, u32, bool) {
    let document = Html::parse_document(html);
    let sel = match Selector::parse("a[href]") {
        Ok(s) => s,
        Err(_) => return (Vec::new(), 0, false),
    };
    let image_sel = match Selector::parse("img") {
        Ok(s) => s,
        Err(_) => return (Vec::new(), 0, false),
    };
    let mut links = Vec::new();
    let mut raw_count = 0u32;
    let mut truncated = false;
    for anchor in document.select(&sel) {
        let href = anchor.value().attr("href").unwrap_or_default();
        if href.is_empty() || href.starts_with('#') || href.starts_with("mailto:") {
            continue;
        }
        raw_count += 1;
        let Some(target_url) = resolve_href(href, page_url) else {
            continue;
        };
        if links.len() as u32 >= max_links {
            truncated = true;
            continue;
        }
        let rel: Vec<String> = anchor
            .value()
            .attr("rel")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let is_nofollow = rel.iter().any(|r| r.eq_ignore_ascii_case("nofollow"));
        let is_image_link = anchor.select(&image_sel).next().is_some();
        let anchor_text = anchor.text().collect::<String>().trim().to_string();
        let anchor_text = if anchor_text.is_empty() && is_image_link {
            anchor
                .select(&image_sel)
                .find_map(|img| img.value().attr("alt"))
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            anchor_text
        };
        links.push(ParsedOutboundLink {
            target_url,
            anchor_text,
            rel,
            is_nofollow,
            is_image_link,
            context: context_from_ancestors(anchor),
        });
    }
    (links, raw_count, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_links_with_context() {
        let html = r#"<html><body><nav><a href="https://skippr.io/about">About</a></nav>
        <a href="/pricing">Pricing</a></body></html>"#;
        let (links, raw, trunc) = parse_html_links("https://example.com/", html, 2000);
        assert!(!trunc);
        assert_eq!(raw, 2);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].context, LinkContext::Nav);
    }

    #[test]
    fn marks_image_links() {
        let html =
            r#"<html><body><a href="/logo"><img alt="Logo" src="/logo.png"></a></body></html>"#;
        let (links, _, _) = parse_html_links("https://example.com/", html, 2000);
        assert_eq!(links.len(), 1);
        assert!(links[0].is_image_link);
        assert_eq!(links[0].anchor_text, "Logo");
    }
}
