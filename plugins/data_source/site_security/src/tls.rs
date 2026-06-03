use serde_json::{json, Value};
use url::Url;

#[derive(Debug, Clone, Default)]
pub struct TlsProbeResult {
    pub http_redirects_to_https: bool,
    pub https_reachable: bool,
    pub cert_valid: bool,
    pub security_txt_found: bool,
    pub message: String,
}

pub async fn probe_origin(site_origin: &str) -> TlsProbeResult {
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return TlsProbeResult {
                message: format!("http client: {e}"),
                ..Default::default()
            };
        }
    };

    let host = site_origin
        .trim_end_matches('/')
        .strip_prefix("https://")
        .or_else(|| site_origin.strip_prefix("http://"))
        .unwrap_or(site_origin);

    let http_url = format!("http://{host}/");
    let https_url = format!("https://{host}/");
    let security_txt = format!("https://{host}/.well-known/security.txt");

    let mut out = TlsProbeResult::default();

    if let Ok(resp) = client.get(&http_url).send().await {
        let final_url = resp.url().to_string();
        out.http_redirects_to_https = final_url.starts_with("https://");
    }

    match client.get(&https_url).send().await {
        Ok(resp) => {
            // reqwest/rustls only returns Ok after validating certificate expiry and hostname.
            out.https_reachable = true;
            out.cert_valid = true;
            out.message = format!("https status {}", resp.status().as_u16());
        }
        Err(e) => {
            out.message = format!("https probe failed: {e}");
            return out;
        }
    }

    if let Ok(resp) = client.get(&security_txt).send().await {
        out.security_txt_found = resp.status().is_success();
    }

    if out.message.is_empty() {
        out.message = "origin probe complete".into();
    }
    out
}

pub fn tls_row(site: &str, run_date: &str, probe: &TlsProbeResult) -> Value {
    json!({
        "site": site,
        "run_date": run_date,
        "http_redirects_to_https": probe.http_redirects_to_https,
        "https_reachable": probe.https_reachable,
        "cert_valid": probe.cert_valid,
        "security_txt_found": probe.security_txt_found,
        "probe_message": probe.message,
    })
}

pub fn site_origin_for_probe(site: &str) -> String {
    if let Ok(u) = Url::parse(site) {
        if let Some(host) = u.host_str() {
            let scheme = u.scheme();
            return format!("{scheme}://{host}");
        }
    }
    if site.starts_with("http://") || site.starts_with("https://") {
        site.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", site.trim_end_matches('/'))
    }
}
