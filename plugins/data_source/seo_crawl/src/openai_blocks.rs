use skippr_plugin_shared_api_source::OpenAiChatClient;

use crate::blocks::ContentBlock;

const SYSTEM_PROMPT: &str = "You analyze SEO content blocks. Respond with JSON only.";

pub async fn analyze_block(
    client: &OpenAiChatClient,
    model: &str,
    site: &str,
    page_url: &str,
    block: &ContentBlock,
) -> Result<serde_json::Value, std::io::Error> {
    let user = format!(
        "site={site} page={page_url} block_id={} type={} text={}",
        block.block_id, block.block_type, block.text.chars().take(500).collect::<String>()
    );
    client.chat_json_object(model, SYSTEM_PROMPT, &user).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::ContentBlock;

    #[tokio::test]
    async fn openai_fixture_returns_json() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::env::set_var("SKIPPR_OPENAI_FIXTURE_DIR", dir);
        let client = OpenAiChatClient::new("fixture-key", "https://api.openai.com/v1");
        let block = ContentBlock {
            block_id: "h1:0".into(),
            block_type: "heading".into(),
            heading: Some("Title".into()),
            text: "Title".into(),
            block_hash: "sha256:1".into(),
        };
        let out = analyze_block(
            &client,
            "gpt-4.1-mini",
            "https://example.com",
            "https://example.com/",
            &block,
        )
        .await
        .expect("fixture openai");
        assert!(out.get("score").is_some());
        std::env::remove_var("SKIPPR_OPENAI_FIXTURE_DIR");
    }
}
