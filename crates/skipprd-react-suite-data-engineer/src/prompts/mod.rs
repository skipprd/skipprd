pub mod ask;
pub mod cleanse;
pub mod ide_agent;
pub mod model;
pub mod patch_contract;
pub mod plan;
pub mod reference;
pub mod review;
pub mod shared;

pub fn ask_system_prompt() -> String {
    ask::system_prompt()
}

pub fn ide_agent_system_prompt() -> String {
    ide_agent::system_prompt()
}

pub fn cleanse_system_prompt() -> String {
    cleanse::cleanse_system_prompt()
}

pub fn cleanse_plan_system_prompt() -> String {
    plan::cleanse_plan_system_prompt()
}

pub fn model_system_prompt() -> String {
    model::model_system_prompt()
}

pub fn model_plan_system_prompt() -> String {
    plan::model_plan_system_prompt()
}

pub fn review_system_prompt() -> String {
    review::system_prompt()
}

pub fn with_time_context(system_prompt: String) -> String {
    let now_utc = chrono::Utc::now().to_rfc3339();
    let now_local = chrono::Local::now();
    let local_iso = now_local.to_rfc3339();
    let local_offset = now_local.offset().to_string();
    format!(
        "{}\n\nTimeContext:\n- NowUTC: {}\n- UserLocal: {} (offset {})",
        system_prompt, now_utc, local_iso, local_offset
    )
}

#[cfg(test)]
mod tests {
    fn assert_no_banned_phrases(label: &str, s: &str) {
        let banned = [
            // Old agent envelope format (must not appear in prompts).
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
            ("ide_agent.system_prompt", super::ide_agent::system_prompt()),
            (
                "cleanse.system_prompt",
                super::cleanse::cleanse_system_prompt(),
            ),
            ("model.system_prompt", super::model::model_system_prompt()),
            (
                "plan.cleanse_plan_system_prompt",
                super::plan::cleanse_plan_system_prompt(),
            ),
            (
                "plan.model_plan_system_prompt",
                super::plan::model_plan_system_prompt(),
            ),
            ("review.system_prompt", super::review::system_prompt()),
            (
                "patch_contract.llm_patch_response_contract",
                super::patch_contract::llm_patch_response_contract().to_string(),
            ),
        ];
        for (label, s) in prompts.iter() {
            assert_no_banned_phrases(label, s);
        }
    }
}
