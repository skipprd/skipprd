/// Canonical patch contracts used across prompts/tools.
///
/// Goal: keep the LLM-facing contract consistent everywhere so patch JSON shapes
/// don't drift between tools (and so strict serde parsing doesn't fail).
///
/// NOTE: This module intentionally does NOT attempt any normalization. If the
/// contract is violated, callers should fail fast with a targeted error.
pub fn llm_patch_response_contract() -> &'static str {
    r#"Patch JSON schema (MUST follow exactly; structs are strict):
- Return ONE JSON object with optional notes and EXACTLY ONE patch payload.
- You MUST return `path` and it MUST equal expected_rel_path.
- You MUST return `patch_text` as Cursor/Aider-style hunks-only unified diff:
  - Starts with '@@'
  - Contains only hunks with -/+ lines (no git file headers like ---/+++ and no diff --git preamble)
  - Hunk headers MUST be Cursor/Aider style: '@@ ... @@' (no line numbers; never '@@ -a,b +c,d @@')
- The patch MUST modify ONLY expected_rel_path.
Output schema:
{
  "notes": ["..."],
  "path": "<expected_rel_path>",
  "patch_text": "@@ ...\n- old\n+ new\n"
}"#
}

pub fn dbt_files_patch_contract() -> &'static str {
    r#"dbt_files file operations contract (MUST follow exactly):
- args.op MUST be one of: "patch" | "rm" | "mv"

op="patch":
- Hard cutover: Cursor/Aider hunks-only unified diff ONLY.
- args: {op:"patch", path:string, patch_text:string}
  - args.path MUST be the single file to mutate.
  - patch_text MUST start with '@@' and MUST NOT include git file headers (---/+++), diff --git preamble, or diffy-style headers ('--- original' / '+++ modified').
  - Hunk headers MUST be Cursor/Aider style: '@@ ... @@' (no line numbers; never '@@ -a,b +c,d @@').
Example args:
{"op":"patch","path":"models/staging/stg_example.sql","patch_text":"@@ ... @@\\n- old\\n+ new\\n"}

op="rm":
- args: {path:string, expected_sha256?:string}
- If expected_sha256 is provided and the file exists, it MUST match the current file sha256.
Example args:
{"op":"rm","path":"models/staging/staging.sql"}

op="mv":
- args: {from:string, to:string, expected_sha256?:string}
- Destination MUST NOT already exist (no implicit overwrite).
- If expected_sha256 is provided, it MUST match the current source file sha256.
Example args:
{"op":"mv","from":"models/staging/foo.sql","to":"models/staging/stg_test_raw_raw_customers.sql"}"#
}

