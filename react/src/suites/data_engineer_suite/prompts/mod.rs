pub fn ask_system_prompt() -> String {
    crate::prompts::ask::system_prompt()
}

pub fn ask_tool_card() -> String {
    crate::prompts::ask::tool_card()
}

pub fn cleanse_system_prompt() -> String {
    crate::prompts::model::model_system_prompt()
}

pub fn cleanse_tool_card() -> String {
    crate::prompts::model::model_tool_card()
}

pub fn model_system_prompt() -> String {
    crate::prompts::model::model_system_prompt()
}

pub fn model_tool_card() -> String {
    crate::prompts::model::model_tool_card()
}

pub fn review_system_prompt() -> String {
    crate::prompts::review::system_prompt()
}

pub fn review_tool_card() -> String {
    crate::prompts::review::tool_card()
}

