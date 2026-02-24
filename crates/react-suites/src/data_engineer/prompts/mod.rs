pub fn ask_system_prompt() -> String {
    crate::prompts::ask::system_prompt()
}

pub fn cleanse_system_prompt() -> String {
    crate::prompts::cleanse::cleanse_system_prompt()
}

pub fn cleanse_plan_system_prompt() -> String {
    crate::prompts::plan::cleanse_plan_system_prompt()
}

pub fn model_system_prompt() -> String {
    crate::prompts::model::model_system_prompt()
}

pub fn model_plan_system_prompt() -> String {
    crate::prompts::plan::model_plan_system_prompt()
}

pub fn review_system_prompt() -> String {
    crate::prompts::review::system_prompt()
}
