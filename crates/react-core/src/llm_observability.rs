use crate::llm::ChatMessage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use once_cell::sync::OnceCell;
use dashmap::DashMap;

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            let vv = v.trim().to_lowercase();
            vv == "1" || vv == "true" || vv == "yes"
        })
        .unwrap_or(false)
}

pub fn llm_calls_enabled() -> bool {
    env_truthy("REACT_LOG_LLM_CALLS") || env_truthy("REACT_LOG_THREAD_STEPS")
}

pub fn llm_response_text_enabled() -> bool {
    env_truthy("REACT_LOG_LLM_RESPONSE_TEXT") || env_truthy("REACT_LOG_THREAD_STEPS")
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn sha256_hex_str(s: &str) -> String {
    sha256_hex_bytes(s.as_bytes())
}

pub fn prompt_hash_for_messages(messages: &[ChatMessage]) -> String {
    // Canonical serialization (stable key order).
    let v: Vec<Value> = messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
        .collect();
    let bytes = serde_json::to_vec(&v).unwrap_or_default();
    sha256_hex_bytes(&bytes)
}

/// Best-effort secret redaction for debug logs and persisted observability.
///
/// Goals:
/// - Avoid leaking common credential/token formats
/// - Preserve as much context as possible for debugging
pub fn redact_common_secrets(input: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in input.lines() {
        let lower = line.to_lowercase();

        // Authorization header (any scheme)
        if lower.contains("authorization:") {
            if let Some((k, _v)) = line.split_once(':') {
                out.push(format!("{}: <redacted>", k.trim()));
                continue;
            }
        }

        // Bearer tokens inline
        if let Some(idx) = lower.find("bearer ") {
            let mut s = line.to_string();
            // Replace token until whitespace/end.
            let start = idx + "bearer ".len();
            let bytes = s.as_bytes();
            let mut end = start;
            while end < bytes.len() {
                let c = bytes[end] as char;
                if c.is_whitespace() {
                    break;
                }
                end += 1;
            }
            if start < end && end <= s.len() {
                s.replace_range(start..end, "<redacted>");
            }
            out.push(s);
            continue;
        }

        // Common JSON keys
        let sensitive_keys = [
            "aws_access_key_id",
            "aws_secret_access_key",
            "aws_session_token",
            "access_token",
            "refresh_token",
            "api_key",
            "x-api-key",
        ];
        let mut replaced = false;
        for k in sensitive_keys.iter() {
            if lower.contains(k) {
                // Very conservative: redact the entire line to avoid partial parsing mistakes.
                out.push(format!("{}: <redacted>", k));
                replaced = true;
                break;
            }
        }
        if replaced {
            continue;
        }

        // AWS access key id patterns (AKIA/ASIA + 16 chars)
        // Best-effort: if present, replace the full token-like span.
        let mut s = line.to_string();
        for prefix in ["AKIA", "ASIA"] {
            let mut search_from = 0usize;
            while let Some(pos) = s[search_from..].find(prefix) {
                let abs = search_from + pos;
                let end = abs + 20; // prefix (4) + 16
                if end <= s.len() && s[abs..end].chars().all(|c| c.is_ascii_alphanumeric()) {
                    s.replace_range(abs..end, "<redacted_aws_access_key_id>");
                    search_from = abs + "<redacted_aws_access_key_id>".len();
                } else {
                    search_from = abs + prefix.len();
                }
            }
        }
        out.push(s);
    }
    out.join("\n")
}

#[derive(Clone, Debug)]
pub struct PartInput {
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct BuiltParts {
    /// Parts in order. Each element is `{name, hash, text}` where text is either full content or `"unchanged"`.
    pub parts: Vec<Value>,
    pub part_hashes: BTreeMap<String, String>,
}

static PART_HASH_CACHE: OnceCell<DashMap<String, String>> = OnceCell::new();
fn part_cache() -> &'static DashMap<String, String> {
    PART_HASH_CACHE.get_or_init(|| DashMap::new())
}

static CALL_ID_CACHE: OnceCell<DashMap<String, u64>> = OnceCell::new();
fn call_id_cache() -> &'static DashMap<String, u64> {
    CALL_ID_CACHE.get_or_init(|| DashMap::new())
}

pub fn next_call_id(thread_id: &str) -> u64 {
    let mut entry = call_id_cache()
        .get(thread_id)
        .map(|v| *v.value())
        .unwrap_or(0);
    entry += 1;
    call_id_cache().insert(thread_id.to_string(), entry);
    entry
}

/// Build deduped parts for a thread. If a part's hash matches the last-seen hash, it is replaced with `"unchanged"`.
///
/// Hashing is performed on the *raw* part text; printed/persisted text is redacted.
pub fn build_parts_for_thread(thread_id: &str, parts: &[PartInput]) -> BuiltParts {
    let mut out_parts: Vec<Value> = Vec::new();
    let mut out_hashes: BTreeMap<String, String> = BTreeMap::new();

    for p in parts.iter() {
        let name = p.name.trim().to_string();
        let raw = p.text.clone();
        let hash = sha256_hex_str(&raw);

        out_hashes.insert(name.clone(), hash.clone());

        let cache_key = format!("{}::{}", thread_id, name);
        let prev = part_cache().get(&cache_key).map(|v| v.value().clone());
        let is_same = prev.as_deref() == Some(hash.as_str());
        part_cache().insert(cache_key, hash.clone());

        if is_same {
            out_parts.push(serde_json::json!({
                "name": name,
                "hash": hash,
                "text": "unchanged"
            }));
        } else {
            out_parts.push(serde_json::json!({
                "name": name,
                "hash": hash,
                "text": redact_common_secrets(&raw)
            }));
        }
    }

    BuiltParts { parts: out_parts, part_hashes: out_hashes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_hash_is_stable_for_same_messages() {
        let msgs = vec![
            ChatMessage { role: "system".to_string(), content: "s".to_string() },
            ChatMessage { role: "user".to_string(), content: "u".to_string() },
        ];
        let h1 = prompt_hash_for_messages(&msgs);
        let h2 = prompt_hash_for_messages(&msgs);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn parts_dedup_marks_unchanged() {
        let tid = "t1";
        let parts = vec![
            PartInput { name: "a".to_string(), text: "hello".to_string() },
            PartInput { name: "b".to_string(), text: "world".to_string() },
        ];
        let first = build_parts_for_thread(tid, &parts);
        assert_eq!(first.parts.len(), 2);
        // First call should include full text.
        assert_eq!(first.parts[0].get("text").and_then(|v| v.as_str()).unwrap(), "hello");

        let second = build_parts_for_thread(tid, &parts);
        assert_eq!(second.parts.len(), 2);
        assert_eq!(second.parts[0].get("text").and_then(|v| v.as_str()).unwrap(), "unchanged");
        assert_eq!(second.parts[1].get("text").and_then(|v| v.as_str()).unwrap(), "unchanged");
    }

    #[test]
    fn redaction_hides_authorization_header() {
        let s = "Authorization: Bearer abcdef\nok";
        let got = redact_common_secrets(s);
        assert!(got.to_lowercase().contains("authorization: <redacted>"));
        assert!(got.contains("ok"));
        assert!(!got.contains("abcdef"));
    }
}
