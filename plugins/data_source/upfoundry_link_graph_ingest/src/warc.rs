use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;

pub struct WarcHtml {
    pub html: String,
    pub used_fixture: bool,
}

/// Fetch HTML for a WARC record. Uses fixture HTML when offset is zero or fixture dir is set.
pub async fn fetch_warc_html(
    fixture_dir: Option<&str>,
    page_url: &str,
    warc_filename: &str,
    offset: i64,
    length: i64,
) -> Result<WarcHtml, String> {
    if let Some(dir) = fixture_dir {
        let path = Path::new(dir).join("pages").join(safe_filename(page_url));
        if let Ok(html) = std::fs::read_to_string(&path) {
            return Ok(WarcHtml {
                html,
                used_fixture: true,
            });
        }
        let generic = Path::new(dir).join("sample.html");
        if let Ok(html) = std::fs::read_to_string(generic) {
            return Ok(WarcHtml {
                html,
                used_fixture: true,
            });
        }
    }
    if offset == 0 && length == 0 {
        return Err("no WARC pointer and no fixture".into());
    }
    let warc_url = format!("https://data.commoncrawl.org/{warc_filename}");
    let client = reqwest::Client::new();
    let end = offset + length - 1;
    let resp = client
        .get(&warc_url)
        .header("Range", format!("bytes={offset}-{end}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("warc fetch status {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let html = extract_html_from_warc_bytes(&bytes).ok_or_else(|| "no html in warc".to_string())?;
    Ok(WarcHtml {
        html,
        used_fixture: false,
    })
}

fn safe_filename(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn extract_html_from_warc_bytes(bytes: &[u8]) -> Option<String> {
    let decompressed = gunzip(bytes).unwrap_or_else(|| bytes.to_vec());
    let text = String::from_utf8_lossy(&decompressed);
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<html")?;
    let end = lower.rfind("</html>")?;
    Some(text[start..end + 7].to_string())
}

fn gunzip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}
