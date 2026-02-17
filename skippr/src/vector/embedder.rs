use crate::llm::LargeLanguageModel;

pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

pub struct LlmEmbedder<T: LargeLanguageModel> {
    llm: T,
}

impl<T: LargeLanguageModel> LlmEmbedder<T> {
    pub fn new(llm: T) -> Self {
        Self { llm }
    }
}

impl<T: LargeLanguageModel> Embedder for LlmEmbedder<T> {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        self.llm.embed(texts)
    }
}
