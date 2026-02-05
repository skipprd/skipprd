#[derive(Clone, Debug)]
pub struct ChunkedDoc {
    pub chunk_id: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ChunkingOptions {
    pub max_tokens: usize,
    pub overlap_tokens: usize,
}

impl Default for ChunkingOptions {
    fn default() -> Self {
        Self {
            max_tokens: 512,
            overlap_tokens: 64,
        }
    }
}

/// Placeholder token-aware chunker; currently splits by characters as a stub.
pub fn chunk_text(doc_id: &str, text: &str, opts: &ChunkingOptions) -> Vec<ChunkedDoc> {
    if text.is_empty() {
        return vec![];
    }
    let step = opts.max_tokens.saturating_sub(opts.overlap_tokens).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let end = (start + opts.max_tokens).min(text.len());
        let slice = &text[start..end];
        let chunk_id = format!("{}:{}-{}", doc_id, start, end);
        chunks.push(ChunkedDoc {
            chunk_id,
            text: slice.to_string(),
        });
        if end == text.len() {
            break;
        }
        start = start.saturating_add(step);
    }
    chunks
}
