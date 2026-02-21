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
- You MUST return `patch_text` as a unified diff.
- You MAY include `path`, but if present it MUST equal expected_rel_path.
- patch_text allowed forms:
  - Cursor-style hunks-only (preferred):
    - Starts with '@@' and contains only hunks with -/+ lines (no ---/+++ headers).
  - Git-style unified diff (also OK):
    - Includes file headers (--- a/<path>, +++ b/<path>) and optional preamble (diff --git ...).
- The patch MUST modify ONLY expected_rel_path (no other files).
Output schema:
{
  "notes": ["..."],
  "path": "<expected_rel_path>" | null,
  "patch_text": "@@ ...\n- old\n+ new\n"
}"#
}

pub fn dbt_files_patch_contract() -> &'static str {
    r#"dbt_files file operations contract (MUST follow exactly):
- args.op MUST be one of: "patch" | "rm" | "mv"

op="patch":
- Hard cutover: Cursor-like patch DSL ONLY. You MUST provide `patch_text` as a unified diff (single-file or multi-file bundle).
- args: {op:"patch", patch_text:string, path?:string}
  - If path is provided, it is a guard: patch_text MUST target exactly that one file.
  - patch_text is allowed in TWO forms:
    - Form A (bundle / explicit headers): git-style file headers per file:
      - Existing file: '--- a/<path>' and '+++ b/<path>'
      - New file:      '--- /dev/null' and '+++ b/<path>'
    - Form B (Cursor-style hunks-only): ONLY when args.path is provided AND patch_text starts with '@@' hunks and omits ---/+++ headers.
      - This form can ONLY patch an existing file (no new file creation).
      - The system will synthesize headers using args.path.
  - For Form A, patch_text MAY include git preamble lines like 'diff --git ...', 'index ...', 'new file mode ...'.
  - patch_text MUST NOT be diffy-style ('--- original' / '+++ modified').
Example args (single-file):
{"op":"patch","patch_text":"diff --git a/models/staging/stg_example.sql b/models/staging/stg_example.sql\\n--- a/models/staging/stg_example.sql\\n+++ b/models/staging/stg_example.sql\\n@@ ..."}
Example args (guarded single-file):
{"op":"patch","path":"models/staging/stg_example.sql","patch_text":"diff --git a/models/staging/stg_example.sql b/models/staging/stg_example.sql\\n--- a/models/staging/stg_example.sql\\n+++ b/models/staging/stg_example.sql\\n@@ ..."}
Example args (Cursor-style hunks-only):
{"op":"patch","path":"models/staging/stg_example.sql","patch_text":"@@ ...\\n- old\\n+ new\\n"}
Example args (multi-file bundle):
{"op":"patch","patch_text":"diff --git a/models/schema.yml b/models/schema.yml\\n--- a/models/schema.yml\\n+++ b/models/schema.yml\\n@@ ...\\n\\ndiff --git a/models/staging/stg_x.sql b/models/staging/stg_x.sql\\n--- /dev/null\\n+++ b/models/staging/stg_x.sql\\n@@ ..."}

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

