#[derive(Clone, Debug, Default)]
pub struct AskOpts {
    pub namespace: Option<String>,
    pub top_k: usize,
    pub use_docs: bool,
    pub use_sql: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub text: String,
    pub followup: Option<String>,
}

/// Stub entrypoint for question answering.
pub fn ask(_question: &str, _opts: &AskOpts) -> Result<Answer, String> {
    Ok(Answer { text: "Question answering not yet implemented".to_string(), followup: None })
}


