pub mod ask;
pub mod cleanse;
pub mod model;
pub mod patch_contract;
pub mod plan;
pub mod reference;
pub mod review;

#[cfg(test)]
mod tests {
    fn assert_no_banned_phrases(label: &str, s: &str) {
        let banned = [
            // Legacy agent envelope (hard-cut removed).
            "{\"action\"",
            // Prompt-level format assertions that conflict with schema-level enforcement.
            "STRICT JSON",
            "Return ONLY JSON",
            "ONLY JSON",
        ];
        for b in banned.iter() {
            assert!(
                !s.contains(b),
                "prompt '{label}' contains banned substring: {b}"
            );
        }
    }

    #[test]
    fn suite_prompts_do_not_include_legacy_or_format_pollution_strings() {
        let prompts: Vec<(&str, String)> = vec![
            ("ask.system_prompt", super::ask::system_prompt()),
            ("ask.tool_card", super::ask::tool_card()),
            ("cleanse.system_prompt", super::cleanse::cleanse_system_prompt()),
            ("cleanse.tool_card", super::cleanse::cleanse_tool_card()),
            ("model.system_prompt", super::model::model_system_prompt()),
            ("model.tool_card", super::model::model_tool_card()),
            ("plan.cleanse_plan_system_prompt", super::plan::cleanse_plan_system_prompt()),
            ("plan.model_plan_system_prompt", super::plan::model_plan_system_prompt()),
            ("review.system_prompt", super::review::system_prompt()),
            ("patch_contract.llm_patch_response_contract", super::patch_contract::llm_patch_response_contract().to_string()),
            ("patch_contract.dbt_files_patch_contract", super::patch_contract::dbt_files_patch_contract().to_string()),
            ("kb.system_prompt", crate::kb::prompts::system_prompt().to_string()),
            ("kb.tool_card", crate::kb::prompts::tool_card().to_string()),
        ];
        for (label, s) in prompts.iter() {
            assert_no_banned_phrases(label, s);
        }
    }
}
