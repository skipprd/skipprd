use skippr_plugin_shared_api_source::OpenAiChatClient;

pub const FIXTURE_ENV: &str = "SKIPPR_AI_CITATIONS_FIXTURE_DIR";

pub const SYSTEM_PROMPT: &str = r#"You answer user questions about products and services on the web.
Respond with JSON only using this schema:
{
  "answer": "full natural language answer",
  "citations": [{"url": "https://...", "title": "optional title", "snippet": "optional excerpt"}],
  "links": [{"url": "https://...", "anchor_text": "optional label"}]
}
Include every URL you rely on in citations and links arrays when possible."#;

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub ok: bool,
    pub answer: Option<String>,
    pub raw_json: Option<serde_json::Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub elapsed_ms: u64,
    pub skipped_unchanged: bool,
}

pub struct CitationClient {
    inner: OpenAiChatClient,
    fixture_dir: Option<String>,
}

impl CitationClient {
    #[cfg(test)]
    pub fn test_client() -> Self {
        Self {
            inner: OpenAiChatClient::new("test-key", "https://api.openai.com/v1"),
            fixture_dir: None,
        }
    }

    pub fn from_config(openai_base_url: Option<&str>) -> Result<Self, std::io::Error> {
        let fixture_dir = std::env::var(FIXTURE_ENV)
            .ok()
            .filter(|d| !d.trim().is_empty());
        let inner = if fixture_dir.is_some() {
            OpenAiChatClient::new(
                "fixture-key",
                openai_base_url.unwrap_or("https://api.openai.com/v1"),
            )
        } else {
            let mut client = OpenAiChatClient::from_env()?;
            if let Some(base) = openai_base_url.filter(|b| !b.trim().is_empty()) {
                client = OpenAiChatClient::new(
                    std::env::var("OPENAI_API_KEY").unwrap_or_default(),
                    base,
                );
            }
            client
        };
        Ok(Self { inner, fixture_dir })
    }

    pub async fn query_prompt(
        &self,
        model: &str,
        prompt_id: &str,
        prompt_text: &str,
        skip_api: bool,
    ) -> QueryResult {
        let started = std::time::Instant::now();
        if skip_api {
            return QueryResult {
                ok: true,
                answer: None,
                raw_json: None,
                error_code: None,
                error_message: None,
                elapsed_ms: started.elapsed().as_millis() as u64,
                skipped_unchanged: true,
            };
        }

        if let Some(dir) = &self.fixture_dir {
            if let Some(body) = load_fixture(dir, prompt_id, model) {
                return parse_success(body, started.elapsed().as_millis() as u64, false);
            }
            let slug = fixture_slug(prompt_id, model);
            return QueryResult {
                ok: false,
                answer: None,
                raw_json: None,
                error_code: Some("fixture_missing".into()),
                error_message: Some(format!(
                    "fixture not found: {}/prompt_{slug}.json",
                    dir.trim_end_matches('/')
                )),
                elapsed_ms: started.elapsed().as_millis() as u64,
                skipped_unchanged: false,
            };
        }

        let user = serde_json::json!({
            "prompt_id": prompt_id,
            "prompt": prompt_text,
        });
        match self
            .inner
            .chat_json_object(model, SYSTEM_PROMPT, &user.to_string())
            .await
        {
            Ok(body) => parse_success(body, started.elapsed().as_millis() as u64, false),
            Err(err) => QueryResult {
                ok: false,
                answer: None,
                raw_json: None,
                error_code: Some("api_error".into()),
                error_message: Some(err.to_string()),
                elapsed_ms: started.elapsed().as_millis() as u64,
                skipped_unchanged: false,
            },
        }
    }
}

fn parse_success(body: serde_json::Value, elapsed_ms: u64, skipped: bool) -> QueryResult {
    let answer = body
        .get("answer")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    QueryResult {
        ok: true,
        answer,
        raw_json: Some(body),
        error_code: None,
        error_message: None,
        elapsed_ms,
        skipped_unchanged: skipped,
    }
}

fn load_fixture(dir: &str, prompt_id: &str, model: &str) -> Option<serde_json::Value> {
    let slug = fixture_slug(prompt_id, model);
    let path = format!("{}/prompt_{slug}.json", dir.trim_end_matches('/'));
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn fixture_slug(prompt_id: &str, model: &str) -> String {
    format!("{}_{}", slugify(prompt_id), slugify(model))
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '.' | '-' | '_') {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    out.trim_matches('_').to_string()
}

pub fn response_hash(answer: &str, raw: Option<&serde_json::Value>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(answer.as_bytes());
    if let Some(json) = raw {
        if let Ok(s) = serde_json::to_string(json) {
            hasher.update(s.as_bytes());
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_slug_is_stable() {
        assert_eq!(
            fixture_slug("best-tools", "gpt-4.1-mini"),
            "best_tools_gpt_4_1_mini"
        );
    }

    #[test]
    fn response_hash_changes_with_answer() {
        let h1 = response_hash("hello", None);
        let h2 = response_hash("world", None);
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn skip_api_returns_unchanged_marker() {
        let client = CitationClient::test_client();
        let result = client
            .query_prompt("gpt-4.1-mini", "p1", "text", true)
            .await;
        assert!(result.ok);
        assert!(result.skipped_unchanged);
        assert!(result.answer.is_none());
    }

    #[tokio::test]
    async fn fixture_missing_returns_error_in_fixture_mode() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        std::env::set_var(FIXTURE_ENV, dir);
        let client = CitationClient::from_config(None).unwrap();
        let result = client
            .query_prompt("gpt-4.1-mini", "no_such_prompt", "q", false)
            .await;
        std::env::remove_var(FIXTURE_ENV);
        assert!(!result.ok);
        assert_eq!(result.error_code.as_deref(), Some("fixture_missing"));
    }

    #[tokio::test]
    async fn fixture_loads_successfully() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        std::env::set_var(FIXTURE_ENV, dir);
        let client = CitationClient::from_config(None).unwrap();
        let result = client
            .query_prompt("gpt-4.1-mini", "best_tools", "q", false)
            .await;
        std::env::remove_var(FIXTURE_ENV);
        assert!(result.ok);
        assert!(result.answer.is_some());
        assert!(result.raw_json.is_some());
    }
}
