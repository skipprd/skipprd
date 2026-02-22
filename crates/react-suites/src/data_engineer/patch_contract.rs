use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SingleFilePatchArgs {
    pub path: String,
    pub patch_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LlmSingleFilePatchResponse {
    pub path: String,
    pub patch_text: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PatchTextNormalization {
    pub patch_text: String,
    pub line_number_headers_rewritten: usize,
}

pub fn single_file_patch_good_example_json() -> &'static str {
    r#"{"op":"patch","path":"models/staging/stg_example.sql","patch_text":"@@ ... @@\n- select old_col from {{ source('raw','events') }}\n+ select new_col from {{ source('raw','events') }}\n"}"#
}

pub fn normalize_hunks_only_patch_text(patch_text: &str) -> Result<PatchTextNormalization, String> {
    let patch_in = patch_text.trim();
    if patch_in.is_empty() {
        return Err("patch_text is empty".to_string());
    }
    if !patch_in.trim_start().starts_with("@@") {
        return Err("patch_text must be Cursor/Aider hunks-only and start with '@@'".to_string());
    }

    let mut out: Vec<String> = Vec::new();
    let mut rewritten = 0usize;
    for line in patch_in.lines() {
        let t = line.trim_start();
        if t.starts_with("diff --git ")
            || t.starts_with("--- ")
            || t.starts_with("+++ ")
            || t.starts_with("index ")
            || t.starts_with("new file mode ")
            || t.starts_with("deleted file mode ")
        {
            return Err(
                "patch_text must be hunks-only (no diff --git/---/+++ headers or git metadata lines)"
                    .to_string(),
            );
        }
        if t.starts_with("@@ -") {
            rewritten = rewritten.saturating_add(1);
            out.push("@@ ... @@".to_string());
        } else {
            out.push(line.to_string());
        }
    }

    Ok(PatchTextNormalization {
        patch_text: out.join("\n"),
        line_number_headers_rewritten: rewritten,
    })
}
