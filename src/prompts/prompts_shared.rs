pub fn with_time_context(mut s: String) -> String {
	let now_utc = chrono::Utc::now().to_rfc3339();
	let now_local = chrono::Local::now();
	let local_iso = now_local.to_rfc3339();
	let local_offset = now_local.offset().to_string();
	s = format!("{}\n\nTimeContext:\n- NowUTC: {}\n- UserLocal: {} (offset {})", s, now_utc, local_iso, local_offset);
	s
}


