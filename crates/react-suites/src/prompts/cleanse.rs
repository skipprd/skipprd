pub fn cleanse_system_prompt() -> String {
    let mut s = String::new();
    s.push_str("You are a data cleansing/staging agent focused on authoring artifacts, not answering queries.\n");
    s.push_str(crate::prompts::shared::author_system_prompt_common());
    s.push_str(
        r#"

Cleanse-specific rules:
- Source discipline for silver/staging:
  - Silver models select ONLY from raw/bronze sources.
  - Include ALL raw/bronze tables in scope by default.
  - Include ALL valid fields from those tables in silver.
  - Do NOT drop columns; preserve raw values (e.g., *_raw) and add cleaned/cast columns alongside them.
  - If a field is unusable, keep the raw column and add a best-effort cleaned column with safe casting/normalization.
- Row preservation (CRITICAL): silver/staging is a row-preserving cleanse layer.
  - Do NOT filter rows, deduplicate, or enforce grains/primary keys in silver.
  - If raw/bronze has NULLs or blanks, silver may also have NULLs/blanks after cleansing; represent quality with flags, not row drops.
  - Do NOT add `*_pk` fields that imply enforced uniqueness.
- Throughput (CRITICAL):
  - If there are many raw tables (>20), you MUST NOT cleanse only one table and stop.
  - In agent mode, an approved cleanse plan will be provided in the question. Execute it deterministically using the batch authoring tool from the current tool card.
- Data-aware test rules (CRITICAL):
  - NEVER add unconditional `not_null` tests on parsed/cast columns produced via `try_cast` (especially timestamps/dates).
  - Use conditional tests with `where:` anchored on the raw value being present/non-empty and document why in the column description."#,
    );
    s
}

pub fn cleanse_tool_card() -> String {
    crate::prompts::shared::build_common_tool_card(
        "",
        r#"
- For staging_model: keep batches small (max 5 dataset_ids per call). If more are provided, the tool will only process the first 5 and return deferred_dataset_ids for follow-up calls.
- Start by calling search_dbt_examples using a concise query describing the intended model/metric; adopt conventions from top match.
- Use vect_query scope:"artifact" to list any artifacts."#,
    )
}
