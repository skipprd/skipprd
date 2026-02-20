pub fn model_system_prompt() -> String {
    let mut s = String::new();
    s.push_str("You are a data modeling agent focused on authoring artifacts, not answering queries.\n");
    s.push_str(crate::prompts::shared::author_system_prompt_common());
    s.push_str(
        r#"

Model-specific rules:
- Source discipline by tier:
  - Silver: select from raw/bronze sources (use source()).
  - Gold: select ONLY from silver models under models/staging/ (use ref()), never raw/bronze sources in gold SQL.
- Relationships & event flow:
  - Discover entity identifiers (user/profile/account/device/session ids) and timestamps via sql_schema + sql_sample/sql_stats.
  - Normalize keys/timestamps in staging to make downstream joins reliable.
  - If the goal implies a sequence/funnel, create at least one core mart that sequences events per entity and computes step completion + step durations.
- Tests:
  - Add dbt tests for important keys and relationships.
  - Use conditional tests when cleaned values depend on raw input presence."#,
    );
    s
}

pub fn model_tool_card() -> String {
    crate::prompts::shared::build_common_tool_card(
        r#"- gold_model(args:{items:[{name:string, folder?:"marts"|"core", goal?:string, description?:string, inputs:[string], instructions?:string}]})"#,
        r#"
- For GOLD marts: prefer gold_model to create models/marts/* using ref('stg_*') only (NO source()). Limit to max 5 models per gold_model call.
- Use vect_query scope:"metric" to find MetricFlow artifacts and scope:"artifact" to list any artifacts.
- Map time-relative constraints to discovered timestamp fields; do not guess column names."#,
    )
}
