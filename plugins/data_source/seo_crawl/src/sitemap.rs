use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Clone, Debug)]
pub struct SitemapEntry {
    pub loc: String,
    pub lastmod: Option<String>,
}

pub fn parse_sitemap_xml(body: &str) -> Result<Vec<SitemapEntry>, std::io::Error> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut entries = Vec::new();
    let mut in_sitemapindex = false;
    let mut current_loc: Option<String> = None;
    let mut current_lastmod: Option<String> = None;
    let mut in_loc = false;
    let mut in_lastmod = false;
    let mut tag_stack = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                tag_stack.push(name.clone());
                if name == "sitemapindex" {
                    in_sitemapindex = true;
                }
                if name == "loc" {
                    in_loc = true;
                }
                if name == "lastmod" {
                    in_lastmod = true;
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().into_owned();
                if in_loc {
                    current_loc = Some(text);
                }
                if in_lastmod {
                    current_lastmod = Some(text);
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "loc" {
                    in_loc = false;
                }
                if name == "lastmod" {
                    in_lastmod = false;
                }
                if name == "url" || (in_sitemapindex && name == "sitemap") {
                    if let Some(loc) = current_loc.take() {
                        entries.push(SitemapEntry {
                            loc,
                            lastmod: current_lastmod.take(),
                        });
                    }
                    current_lastmod = None;
                }
                tag_stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("sitemap XML parse error: {e}"),
                ));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_xml_returns_error() {
        assert!(parse_sitemap_xml("<not-xml").is_err());
    }

    #[test]
    fn parses_urlset() {
        let xml = r#"<?xml version="1.0"?>
        <urlset>
          <url><loc>https://example.com/a</loc><lastmod>2026-01-01</lastmod></url>
        </urlset>"#;
        let entries = parse_sitemap_xml(xml).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].loc, "https://example.com/a");
    }
}
