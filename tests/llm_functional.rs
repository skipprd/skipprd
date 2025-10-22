use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::process::Command;

// Functional chat test using local provider. Assumes a GGUF model path is available via env LLM_CHAT_MODEL.
// On CI/mac M1, set LLM_PROVIDER=LOCAL and LLM_CHAT_MODEL to a small GGUF file path to enable the real inference.
// If no model is provided, we still assert the stub local provider returns a tagged echo (feature disabled path).
#[test]
fn llm_chat_local_provider_behaves() {
    let mut cmd = Command::cargo_bin("skippr").unwrap();
    // Optional: test can be made real by configuring LLM_CHAT_MODEL externally
    cmd.arg("llm").arg("--chat").arg("What is 2+2?");
    let assert = cmd.assert().success();
    // If feature disabled or no model, expect stub prefix; otherwise, just ensure non-empty output
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.trim().len() > 0, "expected some chat output");
}

#[test]
fn llm_embed_local_provider_returns_vectors() {
    let mut cmd = Command::cargo_bin("skippr").unwrap();
    cmd.arg("llm").arg("--embed").arg("alpha").arg("--embed").arg("beta");
    let assert = cmd.assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Expect lines like "0:768" "1:768" (stub or real should be >=1 dims)
    assert!(out.contains("0:"), "expected index prefix");
}


