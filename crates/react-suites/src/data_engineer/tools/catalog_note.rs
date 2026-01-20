use async_trait::async_trait;
use serde_json::Value;

use react_core::agent::AgentCtx;
use react_core::tools::Tool;

pub struct CatalogNoteTool;

fn resolve_single_dataset_id(args: &Value) -> Result<String, String> {
    // Preferred: dataset_id
    if let Some(s) = args.get("dataset_id").and_then(|x| x.as_str()) {
        let t = s.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    // Back/alt compat: dataset_ids with exactly one entry
    if let Some(arr) = args.get("dataset_ids").and_then(|x| x.as_array()) {
        let mut vals: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        vals.dedup();
        if vals.len() == 1 {
            return Ok(vals.remove(0));
        }
        if vals.len() > 1 {
            return Err("catalog_note accepts a single dataset target; provide args.dataset_id OR args.dataset_ids with exactly one item.".to_string());
        }
    }
    Err("dataset_id required (or dataset_ids with exactly one item)".to_string())
}

#[async_trait]
impl Tool for CatalogNoteTool {
    fn name(&self) -> &'static str {
        "catalog_note"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        // Engine-agnostic: attach notes to one dataset ("<catalog>.<database>.<table>").
        let dataset_id = resolve_single_dataset_id(&args)?;
        let field_opt = args.get("field").and_then(|x| x.as_str()).map(|s| s.to_string());
        let text = args.get("text").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        if text.is_empty() {
            return Err("text required".to_string());
        }
        let tags: Vec<String> = args
            .get("tags")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let preview = args.get("preview").and_then(|x| x.as_bool()).unwrap_or(false);
        let thread_id = ctx.thread_id.clone().unwrap_or_default();

        // Resolve catalog key
        let catalog_key = ctx.keyspace.catalog_key(&ctx.scope, &dataset_id);
        let mut catalog: Value = ctx
            .storage
            .get_json(&catalog_key)
            .await
            .map_err(|e| format!("get_json: {}", e))?;

        // Extract existing facts
        let mut existing_facts: Vec<String> = Vec::new();
        if let Some(fld) = field_opt.as_ref() {
            if let Some(fields) = catalog.get_mut("fields").and_then(|x| x.as_array()) {
                for f in fields {
                    if f.get("name").and_then(|x| x.as_str()) == Some(fld.as_str()) {
                        if let Some(gn) = f
                            .get("governance_notes")
                            .and_then(|g| g.get("facts"))
                            .and_then(|x| x.as_array())
                        {
                            for it in gn {
                                if let Some(s) = it.as_str() {
                                    existing_facts.push(s.to_string());
                                }
                            }
                        }
                        break;
                    }
                }
            }
        } else if let Some(gn) = catalog
            .get("governance_notes")
            .and_then(|g| g.get("dataset"))
            .and_then(|x| x.get("facts"))
            .and_then(|x| x.as_array())
        {
            for it in gn {
                if let Some(s) = it.as_str() {
                    existing_facts.push(s.to_string());
                }
            }
        }

        // Curate digest using LLM with governance language
        let sys = "You are a careful data governance curator. Produce a concise, non-duplicative digest with ≤8 bullets (≤280 chars each) capturing only factual, high-signal additions. Avoid repetition and speculation.";
        let prompt = format!(
            "Existing facts:\n{}\n\nNew contribution:\n{}\n\nWrite curated bullets only. Use a tone of a considered, sentient update from a fastidious custodian of data governance.",
            existing_facts.iter().map(|s| format!("- {}", s)).collect::<Vec<_>>().join("\n"),
            text
        );
        let llm = ctx.llm.clone();
        // Track token expense (chars only)
        let chat_in_chars: usize = sys.len() + prompt.len();
        let bullets_text = match llm.chat(&[
            react_core::llm::ChatMessage {
                role: "system".to_string(),
                content: sys.to_string(),
            },
            react_core::llm::ChatMessage {
                role: "user".to_string(),
                content: prompt.clone(),
            },
        ]) {
            Ok(s) => s,
            Err(_) => text.clone(),
        };
        let chat_out_chars: usize = bullets_text.len();

        // Parse bullets (lines starting with - or *)
        let mut proposed: Vec<String> = Vec::new();
        for line in bullets_text.lines() {
            let t = line
                .trim()
                .trim_start_matches('-')
                .trim_start_matches('*')
                .trim();
            if !t.is_empty() {
                proposed.push(t.to_string());
            }
            if proposed.len() >= 8 {
                break;
            }
        }

        // Deduplicate by embeddings cosine similarity
        let embed = |src: &[String]| -> Vec<Vec<f32>> {
            if src.is_empty() {
                return Vec::new();
            }
            llm.embed(&src.to_vec()).unwrap_or_default()
        };
        let existing_vecs = embed(&existing_facts);
        let proposed_vecs = embed(&proposed);
        let mut curated: Vec<String> = Vec::new();
        for (i, cand) in proposed.iter().enumerate() {
            let v = proposed_vecs.get(i);
            let mut dup = false;
            if let Some(vv) = v {
                for ev in existing_vecs.iter() {
                    let c = cosine(vv, ev);
                    if c >= 0.92 {
                        dup = true;
                        break;
                    }
                }
            }
            if !dup {
                curated.push(cand.clone());
            }
            if curated.len() >= 8 {
                break;
            }
        }

        let digest = curated
            .get(0)
            .cloned()
            .unwrap_or_else(|| proposed.get(0).cloned().unwrap_or_else(|| text.chars().take(200).collect()));
        if preview {
            let est_tokens = ((chat_in_chars + chat_out_chars) as f32 / 4.0).round() as i64;
            return Ok(serde_json::json!({"ok": true, "preview": { "add": curated, "remove": [], "digest": digest }, "llm_expense": {"chat_chars_in": chat_in_chars, "chat_chars_out": chat_out_chars, "est_tokens": est_tokens}}));
        }

        // Merge into catalog JSON
        let now = chrono::Utc::now().to_rfc3339();
        let prov = serde_json::json!({"source":"user","tags":tags,"ts":now,"thread_id":thread_id});
        if let Some(fld) = field_opt.as_ref() {
            if let Some(fields) = catalog.get_mut("fields").and_then(|x| x.as_array_mut()) {
                for f in fields {
                    if f.get("name").and_then(|x| x.as_str()) == Some(fld.as_str()) {
                        let obj = f.as_object_mut().unwrap();
                        let gn = obj.entry("governance_notes").or_insert(serde_json::json!({ "facts": [], "digest": "", "provenance": [], "last_updated": "" }));
                        merge_notes(gn, &curated, &digest, &prov, &now);
                        break;
                    }
                }
            }
        } else {
            let gn_parent = catalog
                .as_object_mut()
                .unwrap()
                .entry("governance_notes")
                .or_insert(serde_json::json!({ "dataset": { "facts": [], "digest": "", "provenance": [], "last_updated": "" }}));
            let ds = gn_parent.get_mut("dataset").unwrap();
            merge_notes(ds, &curated, &digest, &prov, &now);
        }

        // Write back atomically
        ctx.storage
            .put_json(&catalog_key, &catalog)
            .await
            .map_err(|e| format!("put_json: {}", e))?;

        // Upsert curated digest into embeddings as a doc
        let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;
        let epoch = chrono::Utc::now().timestamp() as u64;
        let mut vec1: Vec<f32> = Vec::new();
        if let Ok(vv) = llm.embed(&[digest.clone()]) {
            if let Some(v) = vv.get(0) {
                vec1 = v.clone();
            }
        }
        if !vec1.is_empty() {
            let id = format!("doc:{}:catalog_note", dataset_id);
            let chunk = react_core::providers::VectorChunk {
                id,
                kind: "doc".to_string(),
                dataset_id: dataset_id.clone(),
                field: field_opt.clone(),
                text: digest.clone(),
                vector: vec1,
                meta: serde_json::json!({"scope":"catalog", "level": if field_opt.is_some() { "field" } else { "dataset" }, "tags": tags }),
                epoch,
            };
            let _ = vector.upsert(&ctx.scope, &[chunk]).await;
        }

        let est_tokens = ((chat_in_chars + chat_out_chars) as f32 / 4.0).round() as i64;
        Ok(serde_json::json!({"ok": true, "curated": { "digest": digest, "facts": curated }, "key": catalog_key, "llm_expense": {"chat_chars_in": chat_in_chars, "chat_chars_out": chat_out_chars, "est_tokens": est_tokens}}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_single_dataset_id_accepts_dataset_id() {
        let args = serde_json::json!({"dataset_id":"AwsDataCatalog.test_raw.raw_customers"});
        let got = resolve_single_dataset_id(&args).expect("ok");
        assert_eq!(got, "AwsDataCatalog.test_raw.raw_customers");
    }

    #[test]
    fn resolve_single_dataset_id_accepts_dataset_ids_len1() {
        let args = serde_json::json!({"dataset_ids":["AwsDataCatalog.test_raw.raw_customers"]});
        let got = resolve_single_dataset_id(&args).expect("ok");
        assert_eq!(got, "AwsDataCatalog.test_raw.raw_customers");
    }

    #[test]
    fn resolve_single_dataset_id_rejects_dataset_ids_len_gt1() {
        let args = serde_json::json!({"dataset_ids":["a.b.c","d.e.f"]});
        let err = resolve_single_dataset_id(&args).unwrap_err();
        assert!(err.contains("exactly one"));
    }
}

fn cosine(a: &Vec<f32>, b: &Vec<f32>) -> f32 {
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    let len = a.len().min(b.len());
    for i in 0..len {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn merge_notes(obj: &mut Value, curated: &[String], digest: &str, provenance: &Value, now: &str) {
    let o = obj.as_object_mut().unwrap();
    // facts
    let facts = o.entry("facts").or_insert(serde_json::json!([])).as_array_mut().unwrap();
    for c in curated {
        if !facts.iter().any(|v| v.as_str() == Some(c.as_str())) {
            facts.push(Value::String(c.clone()));
        }
    }
    // digest (replace with latest curated message)
    o.insert("digest".to_string(), Value::String(digest.to_string()));
    // provenance log
    let prov = o
        .entry("provenance")
        .or_insert(serde_json::json!([]))
        .as_array_mut()
        .unwrap();
    prov.push(provenance.clone());
    // timestamp
    o.insert("last_updated".to_string(), Value::String(now.to_string()));
}

