/// Canonical patch contracts used across prompts/tools.
///
/// Goal: keep the LLM-facing contract consistent everywhere so patch JSON shapes
/// don't drift between tools (and so strict serde parsing doesn't fail).
///
/// NOTE: This module intentionally does NOT attempt any normalization. If the
/// contract is violated, callers should fail fast with a targeted error.
pub fn llm_patch_response_contract() -> &'static str {
    r#"Patch JSON schema (MUST follow exactly; structs are strict):
- Return ONE JSON object with optional notes and EXACTLY ONE patch primitive.
- Use the key name `new_text` (NOT content/text/newText).
- You MAY include `path`, but if present it MUST equal expected_rel_path.
Output schema:
{
  "notes": ["..."],
  "replace_file": {"path":"<expected_rel_path>","new_text":"..."} | null,
  "replace_range": {"path":"<expected_rel_path>","start_line":1,"end_line":1,"new_text":"..."} | null,
  "replace_list": {"path":"<expected_rel_path>","edits":[{"start_line":1,"end_line":1,"new_text":"..."}]} | null
}
(Exactly ONE of replace_file/replace_range/replace_list must be non-null; the others must be null.)"#
}

pub fn dbt_files_patch_contract() -> &'static str {
    r#"dbt_files(op=patch) contract (MUST follow exactly):
- Call shape: {"action":"dbt_files","args":{...}}
- args.op MUST be "patch"
- args MUST include EXACTLY ONE of:
  - replace_file: {path:string, new_text:string, expected_sha256?:string} | [{...}]
  - replace_range: {path:string, start_line:int, end_line:int, new_text:string, expected_sha256?:string} | [{...}]
  - replace_list: {path:string, edits:[{start_line:int, end_line:int, new_text:string}], expected_sha256?:string} | [{...}]
- If expected_sha256 is provided, it MUST match the current file content sha256.
- Only include fields shown above; the patch structs are strict and extra keys will fail parsing.
Example (replace_file):
{"action":"dbt_files","args":{"op":"patch","replace_file":{"path":"models/staging/stg_example.sql","new_text":"-- sql...","expected_sha256":"<sha256>"}}}"#
}

