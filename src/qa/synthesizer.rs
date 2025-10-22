#[derive(Clone, Debug, Default)]
pub struct SynthesisInput {
    pub doc_snippets: Vec<String>,
    pub sql_rows: Vec<String>,
}

pub fn synthesize_answer(_input: &SynthesisInput) -> Result<String, String> {
    Ok("Synthesis not yet implemented".to_string())
}


