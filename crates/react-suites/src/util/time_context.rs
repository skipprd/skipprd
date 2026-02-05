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
