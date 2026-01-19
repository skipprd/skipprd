use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};

use react_core::agent::AgentCtx;
use react_core::providers::VectorChunk;
use react_core::tools::Tool;

pub struct KbIngestDirTool;

fn is_text_file(path: &Path) -> bool {
    match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase().as_str() {
        "txt" | "md" | "markdown" | "rst" => true,
        _ => false,
    }
}

fn safe_rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .ok()
        .map(|rp| rp.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().to_string())
        .replace('\\', "/")
}

fn chunk_text(text: &str, chunk_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    for para in text.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        if buf.len() + p.len() + 2 > chunk_chars && !buf.is_empty() {
            out.push(buf.trim().to_string());
            buf.clear();
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(p);
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    // Fallback: if we produced nothing (e.g., single huge line), split hard.
    if out.is_empty() && !text.trim().is_empty() {
        let t = text.trim();
        let chars: Vec<char> = t.chars().collect();
        let mut start = 0usize;
        while start < chars.len() {
            let end = (start + chunk_chars).min(chars.len());
            out.push(chars[start..end].iter().collect());
            start = end;
        }
    }
    out
}

#[async_trait]
impl Tool for KbIngestDirTool {
    fn name(&self) -> &'static str {
        "kb_ingest_dir"
    }

    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        let dir = args
            .get("dir")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "missing field `dir`".to_string())?;
        let dataset_id = args
            .get("dataset_id")
            .and_then(|x| x.as_str())
            .unwrap_or("kb")
            .to_string();
        let max_files = args.get("max_files").and_then(|x| x.as_u64()).unwrap_or(200) as usize;
        let max_bytes = args.get("max_bytes").and_then(|x| x.as_u64()).unwrap_or(2_000_000) as usize;
        let chunk_chars = args.get("chunk_chars").and_then(|x| x.as_u64()).unwrap_or(1200) as usize;

        let root = PathBuf::from(dir);
        if !root.exists() {
            return Err(format!("dir does not exist: {}", dir));
        }
        if !root.is_dir() {
            return Err(format!("not a directory: {}", dir));
        }

        let vector = ctx.vector.as_ref().ok_or_else(|| "vector provider missing".to_string())?;

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false).into_iter().filter_map(|e| e.ok()) {
            if files.len() >= max_files {
                break;
            }
            let p = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            if !is_text_file(p) {
                continue;
            }
            // size guard
            if let Ok(md) = std::fs::metadata(p) {
                if md.len() as usize > max_bytes {
                    continue;
                }
            }
            files.push(p.to_path_buf());
        }

        if files.is_empty() {
            return Ok(serde_json::json!({"ok": true, "dataset_id": dataset_id, "ingested_files": 0, "ingested_chunks": 0, "note": "no eligible files found"}));
        }

        let epoch = chrono::Utc::now().timestamp() as u64;
        let mut chunks: Vec<VectorChunk> = Vec::new();

        for p in files.iter() {
            let rel = safe_rel(&root, p);
            let bytes = std::fs::read(p).map_err(|e| format!("read {}: {}", rel, e))?;
            let text = String::from_utf8_lossy(&bytes).to_string();
            for (i, part) in chunk_text(&text, chunk_chars).into_iter().enumerate() {
                let doc_text = format!("file: {}\n\n{}", rel, part);
                chunks.push(VectorChunk {
                    id: format!("doc:{}:{}:{}", dataset_id, rel, i),
                    kind: "doc".to_string(),
                    dataset_id: dataset_id.clone(),
                    field: None,
                    text: doc_text,
                    vector: Vec::new(),
                    meta: serde_json::json!({"path": rel, "chunk_index": i}),
                    epoch,
                });
            }
        }

        // Embed in batches
        let batch = 64usize;
        let mut idx = 0usize;
        while idx < chunks.len() {
            let end = (idx + batch).min(chunks.len());
            let texts: Vec<String> = chunks[idx..end].iter().map(|c| c.text.clone()).collect();
            let vecs = ctx.llm.embed(&texts).map_err(|e| format!("embed failed: {}", e))?;
            for (k, v) in vecs.into_iter().enumerate() {
                chunks[idx + k].vector = v;
            }
            idx = end;
        }

        vector.upsert(&ctx.scope, &chunks).await?;

        Ok(serde_json::json!({
            "ok": true,
            "dataset_id": dataset_id,
            "ingested_files": files.len(),
            "ingested_chunks": chunks.len()
        }))
    }
}

