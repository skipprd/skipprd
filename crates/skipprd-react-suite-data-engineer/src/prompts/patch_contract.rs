/// Canonical patch contract (semantic rules only — JSON structure is enforced by the schema).
pub fn llm_patch_response_contract() -> String {
    format!(
        r#"Patch rules:
- `path` MUST equal expected_rel_path.
- `patch_text` MUST be Cursor/Aider-style hunks-only unified diff:
  - Starts with '@@'
  - Contains only hunks with -/+ lines (no git file headers like ---/+++, no diff --git preamble, and no *** Begin Patch envelope)
  - Hunk headers MUST be Cursor/Aider style: '@@ ... @@' (no line numbers; never '@@ -a,b +c,d @@')
- The patch MUST modify ONLY expected_rel_path.
Good args example:
{}"#,
        crate::patch_contract::single_file_patch_good_example_json()
    )
}
