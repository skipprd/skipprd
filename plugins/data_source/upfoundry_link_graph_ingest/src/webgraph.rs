use std::io::Read;
use std::path::Path;

use aws_sdk_s3::Client;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skippr_plugin_shared_link_graph::domain_id;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainAuthorityRow {
    pub domain_id: u64,
    pub domain: String,
    pub rank_percentile: Option<f64>,
    pub pagerank: Option<f64>,
    pub harmonic_centrality: Option<f64>,
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebGraphImportStats {
    pub mode: String,
    pub source_uri: Option<String>,
    pub rows_imported: u32,
    pub errors: Vec<String>,
}

pub async fn import_web_graph_priors(
    client: &Client,
    bucket: &str,
    root: &str,
    source_uri: Option<&str>,
    fixture_dir: Option<&str>,
    max_rows: u32,
) -> Result<WebGraphImportStats, std::io::Error> {
    let Some((mode, uri, bytes)) = load_source(client, source_uri, fixture_dir).await? else {
        return Ok(WebGraphImportStats {
            mode: "not_configured".into(),
            ..Default::default()
        });
    };
    let decoded = maybe_gunzip(&bytes);
    let text = String::from_utf8_lossy(&decoded);
    let mut rows = Vec::new();
    let mut stats = WebGraphImportStats {
        mode,
        source_uri: Some(uri),
        ..Default::default()
    };
    for line in text.lines() {
        if rows.len() as u32 >= max_rows {
            break;
        }
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        match parse_rank_line(line) {
            Some(row) => rows.push(row),
            None => {
                if stats.errors.len() < 10 {
                    stats.errors.push(format!("unparseable row: {line}"));
                }
            }
        }
    }
    stats.rows_imported = rows.len() as u32;
    if !rows.is_empty() {
        put_jsonl(
            client,
            bucket,
            &format!("{root}cc/webgraph/domain_ranks/current.jsonl"),
            &rows,
        )
        .await?;
        put_jsonl(
            client,
            bucket,
            &format!("{root}authority/domain_ranks/current.jsonl"),
            &rows,
        )
        .await?;
    }
    Ok(stats)
}

async fn load_source(
    client: &Client,
    source_uri: Option<&str>,
    fixture_dir: Option<&str>,
) -> Result<Option<(String, String, Vec<u8>)>, std::io::Error> {
    if let Some(dir) = fixture_dir {
        let path = Path::new(dir).join("webgraph_domain_ranks.jsonl");
        if path.exists() {
            return Ok(Some((
                "fixture".into(),
                path.display().to_string(),
                std::fs::read(path)?,
            )));
        }
    }
    let Some(uri) = source_uri.filter(|uri| !uri.trim().is_empty()) else {
        return Ok(None);
    };
    if let Some(path) = uri.strip_prefix("s3://") {
        let Some((bucket, key)) = path.split_once('/') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid s3 URI: {uri}"),
            ));
        };
        let resp = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?
            .into_bytes()
            .to_vec();
        return Ok(Some(("s3".into(), uri.to_string(), bytes)));
    }
    if uri.starts_with("https://") || uri.starts_with("http://") {
        let bytes = reqwest::get(uri)
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?
            .bytes()
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))?
            .to_vec();
        return Ok(Some(("http".into(), uri.to_string(), bytes)));
    }
    Ok(Some(("file".into(), uri.to_string(), std::fs::read(uri)?)))
}

fn parse_rank_line(line: &str) -> Option<DomainAuthorityRow> {
    if let Ok(value) = serde_json::from_str::<Value>(line) {
        let domain = string_field(
            &value,
            &[
                "domain",
                "host",
                "host_name",
                "domain_name",
                "url_host_registered_domain",
            ],
        )?;
        return row_from_parts(
            &domain,
            number_field(&value, &["rank_percentile", "percentile"]),
            number_field(&value, &["pagerank", "page_rank", "rank"]),
            number_field(&value, &["harmonic_centrality", "harmonic"]),
            "cc_web_graph",
        );
    }
    let delimiter = if line.contains('\t') { '\t' } else { ',' };
    let parts = line
        .split(delimiter)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    if parts.len() >= 5 && parts[0].parse::<u64>().is_ok() && parts[2].parse::<u64>().is_ok() {
        return row_from_parts(
            &reverse_domain(parts[4]),
            None,
            parts[3].parse::<f64>().ok(),
            parts[1].parse::<f64>().ok(),
            "cc_web_graph_rank_file",
        );
    }
    let domain = parts[0];
    let first_number = parts.get(1).and_then(|part| part.parse::<f64>().ok());
    let second_number = parts.get(2).and_then(|part| part.parse::<f64>().ok());
    let (rank_percentile, pagerank) = match (first_number, second_number) {
        (Some(first), Some(second)) if first > 1.0 && second <= 1.0 => (Some(first), Some(second)),
        (Some(first), Some(second)) => (Some(second), Some(first)),
        (Some(first), None) if first <= 1.0 => (None, Some(first)),
        (Some(first), None) => (Some(first), None),
        _ => (None, None),
    };
    row_from_parts(domain, rank_percentile, pagerank, None, "cc_web_graph")
}

fn row_from_parts(
    domain: &str,
    rank_percentile: Option<f64>,
    pagerank: Option<f64>,
    harmonic_centrality: Option<f64>,
    source: &str,
) -> Option<DomainAuthorityRow> {
    let domain = normalize_domain(domain)?;
    Some(DomainAuthorityRow {
        domain_id: domain_id(&domain),
        domain,
        rank_percentile,
        pagerank,
        harmonic_centrality,
        source: source.into(),
    })
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::to_string)
}

fn number_field(value: &Value, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_f64))
}

fn normalize_domain(value: &str) -> Option<String> {
    let trimmed = value
        .trim()
        .trim_end_matches('.')
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if trimmed.contains('.') && trimmed.chars().any(|c| c.is_ascii_alphabetic()) {
        Some(trimmed)
    } else {
        None
    }
}

fn reverse_domain(value: &str) -> String {
    value
        .split('.')
        .filter(|part| !part.trim().is_empty())
        .rev()
        .collect::<Vec<_>>()
        .join(".")
}

fn maybe_gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    if decoder.read_to_end(&mut out).is_ok() {
        out
    } else {
        bytes.to_vec()
    }
}

async fn put_jsonl<T: Serialize>(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[T],
) -> Result<(), std::io::Error> {
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
        body.push('\n');
    }
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .content_type("application/x-ndjson")
        .body(aws_sdk_s3::primitives::ByteStream::from(body.into_bytes()))
        .send()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_rank_row() {
        let row =
            parse_rank_line(r#"{"domain":"example.com","rank_percentile":12.5,"pagerank":0.42}"#)
                .unwrap();
        assert_eq!(row.domain, "example.com");
        assert_eq!(row.rank_percentile, Some(12.5));
        assert_eq!(row.pagerank, Some(0.42));
    }

    #[test]
    fn parses_csv_rank_row() {
        let row = parse_rank_line("example.com,0.42,12.5").unwrap();
        assert_eq!(row.domain, "example.com");
        assert_eq!(row.rank_percentile, Some(12.5));
        assert_eq!(row.pagerank, Some(0.42));
    }

    #[test]
    fn parses_common_crawl_rank_row() {
        let row =
            parse_rank_line("1\t3.1238044E7\t3\t0.01110707704411023\tcom.facebook\t3632").unwrap();
        assert_eq!(row.domain, "facebook.com");
        assert_eq!(row.pagerank, Some(0.01110707704411023));
        assert_eq!(row.harmonic_centrality, Some(3.1238044E7));
    }
}
