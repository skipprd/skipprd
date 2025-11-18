use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use crate::qa::agent::{Agent, AgentCtx, RunOutcome};
use crate::qa::tools::{ToolRegistry};
use crate::qa::tools::{sql_run::SqlRunTool, sql_schema::SqlSchemaTool, sql_stats::SqlStatsTool, sql_sample::SqlSampleTool, vect_query::VectQueryTool, ask_user::AskUserTool, approve_save::ApproveAndSaveArtifactTool, artifacts::ArtifactsTool};
use uuid::Uuid;
use crate::ws::api_gen as api;
use std::collections::{HashMap, VecDeque};
use chrono::Utc;
use crate::models as m;
use crate::qa::session::ThreadStore;

pub async fn start(port: u16) -> Result<(), String> {
    // Ensure configuration is loaded so tenant/workspace are correct for this process
    crate::helpers::configuration::Config::build_config();
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await.map_err(|e| e.to_string())?;
    tracing::info!("WebSocket server listening on ws://{}", addr);
    loop {
        let (stream, _sockaddr) = listener.accept().await.map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            if let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await {
                let (mut write, mut read) = ws_stream.split();
				let mut state = ConnState::new();
                while let Some(msg) = read.next().await {
                    match msg {
						Ok(Message::Text(txt)) => {
							tracing::info!("WS <- {}", txt);
							// Fast-path new/open to stream initial frames immediately
							if let Ok(v) = serde_json::from_str::<Value>(&txt) {
								if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
									if t == "new" {
										if let Err(e) = process_new(&v, &mut state, &mut write).await {
											let cid_guess = v.get("cid").and_then(|x| x.as_str()).map(|s| s.to_string());
											let mut err = api::ErrorResponse::new(1, m::error_response::Type::Error, now_iso(), e.clone());
											err.code = Some("invalid_request".to_string());
											err.cid = cid_guess;
											let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
											tracing::info!("WS -> {}", s);
											let _ = write.send(Message::Text(s)).await;
										}
										continue;
									} else if t == "open" {
										if let Err(e) = process_open(&v, &mut state, &mut write).await {
											let cid_guess = v.get("cid").and_then(|x| x.as_str()).map(|s| s.to_string());
											let mut err = api::ErrorResponse::new(1, m::error_response::Type::Error, now_iso(), e.clone());
											err.code = Some("invalid_request".to_string());
											err.cid = cid_guess;
											let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
											tracing::info!("WS -> {}", s);
											let _ = write.send(Message::Text(s)).await;
										}
										continue;
									}
								}
							}
							// Other types: handle and send after processing
							match handle_message(&txt, &mut state).await {
								Ok(frames) => {
									for f in frames {
										tracing::info!("WS -> {}", f);
										let _ = write.send(Message::Text(f)).await;
									}
								}
								Err(e) => {
									let cid_guess = serde_json::from_str::<Value>(&txt)
										.ok()
										.and_then(|vv| vv.get("cid").and_then(|x| x.as_str()).map(|s| s.to_string()));
									let mut err = api::ErrorResponse::new(1, m::error_response::Type::Error, now_iso(), e.clone());
									err.code = Some("invalid_request".to_string());
									err.cid = cid_guess;
									let s = serde_json::to_string(&err)
										.unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
									tracing::info!("WS -> {}", s);
									let _ = write.send(Message::Text(s)).await;
								}
							}
						}
                        Ok(Message::Close(_)) => break,
                        _ => {}
                    }
                }
            }
        });
    }
}

async fn handle_message(text: &str, state: &mut ConnState) -> Result<Vec<String>, String> {
	let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
	let typ = v.get("type").and_then(|x| x.as_str()).ok_or_else(|| "missing field `type`".to_string())?;
	let mut out: Vec<String> = Vec::new();
	match typ {
		"list" => {
			let _req: api::ListRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let store = crate::qa::session::ThreadStore::new();
			let ids = store.list().await;
			let mut threads: Vec<api::ListResponseThreadsInner> = Vec::new();
			for tid in ids {
				let mut item = api::ListResponseThreadsInner::new(tid.clone());
				if let Some(log) = store.get(&tid).await {
					// last_activity
					let last_ts = log.steps.last().map(|s| s.ts.clone());
					item.last_activity = last_ts;
					item.title = log.title.clone();
					// compute preview from last user or final
					let mut preview: Option<String> = None;
					for step in log.steps.iter().rev() {
						if step.action == "user" {
							if let Some(txt) = step.args.get("text").and_then(|x| x.as_str()) {
								preview = Some(txt.to_string()); break;
							}
						}
						if step.action == "final" {
							if let Some(ans) = step.args.get("answer").and_then(|x| x.as_str()) {
								preview = Some(ans.to_string()); break;
							}
						}
					}
					item.last_message_preview = preview;
					// unread count based on seen map vs assistant messages count
					let seen = state.seen.get(&tid).copied().unwrap_or(0);
					let (assistant_seq_max, assistant_after_seen) = compute_unread_for_log(&log, seen);
					let unread = assistant_after_seen;
					item.unread_count = Some(unread);
					// ensure thread_seq map is at least assistant_seq_max
					let entry = state.thread_seq.entry(tid.clone()).or_insert(0);
					if *entry < assistant_seq_max { *entry = assistant_seq_max; }
				}
				threads.push(item);
			}
			let resp = api::ListResponse::new(1, m::list_response::Type::List, now_iso(), state.next_seq(), threads);
			out.push(serde_json::to_string(&resp).unwrap_or_else(|_| "{\"type\":\"error\",\"error\":\"serialization error\"}".to_string()));
			state.buffer_last(&out[out.len()-1]);
		}
		"new" => {
			let req: api::NewRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let cid = req.cid.clone();
			let question = req.question.clone();
			if question.trim().is_empty() { return Err("question required".into()); }
			let thread_id = Uuid::new_v4().to_string();
			let agent = normalize_agent(req.agent_type.clone());
			state.current_agent.insert(thread_id.clone(), agent.clone());
			// ok
			let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
			ok.cid = Some(cid.clone());
			out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
			// thread_assigned
			let ta = api::ServerMessage::ThreadAssigned(api::ThreadAssignedResponse::new(1, m::thread_assigned_response::Type::ThreadAssigned, now_iso(), state.next_seq(), cid.clone(), thread_id.clone()));
			let ta_s = serde_json::to_string(&ta).unwrap();
			state.buffer_last(&ta_s);
			out.push(ta_s);
			// processing (reasoning)
			let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.clone());
			pr.for_cid = Some(cid.clone());
			pr.stage = Some(m::processing_response::Stage::Queued);
			pr.progress = Some(0.0);
			let prm = api::ServerMessage::Processing(pr);
			let pr_s = serde_json::to_string(&prm).unwrap();
			state.buffer_last(&pr_s);
			out.push(pr_s);
			// record initial user question in thread history
			{
				let store = crate::qa::session::ThreadStore::new();
				let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
					action: "user".to_string(),
					args: serde_json::json!({"text": question}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(agent.clone()),
				}).await;
				let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
			}
			// run agent
			let frames = run_agent_and_frames(&thread_id, &question, &agent).await?;
			for f in frames {
				match f {
					AgentFrame::Final { answer, sql } => {
						// finalize title once using concise summary
						{
							let store = crate::qa::session::ThreadStore::new();
							let title = synthesize_title(&question, &answer).await;
							let _ = store.finalize_title(&thread_id, &title).await;
						}
						// optional token streaming (simple chunking)
						for t in chunk_text(&answer, 24) {
							let mut tk = api::TokenResponse::new(1, m::token_response::Type::Token, now_iso(), state.next_seq(), thread_id.clone(), t);
							tk.for_cid = Some(cid.clone());
							let m = api::ServerMessage::Token(tk);
							let s = serde_json::to_string(&m).unwrap();
							state.buffer_last(&s);
							out.push(s);
						}
						let tseq = state.next_thread_seq(&thread_id);
						let resp = api::ServerMessage::Final(api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.clone(), tseq, api::FinalResponseResult { sql, answer }));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// Log entire thread on final
						{
							let store = crate::qa::session::ThreadStore::new();
							if let Some(log) = store.get(&thread_id).await {
								if let Ok(pretty) = serde_json::to_string_pretty(&log) {
									tracing::info!("{}", pretty);
								}
							}
						}
					}
					AgentFrame::AwaitUser { prompt } => {
						let tseq = state.next_thread_seq(&thread_id);
						let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(1, m::await_user_response::Type::AwaitUser, now_iso(), state.next_seq(), thread_id.clone(), tseq, prompt));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
					}
				}
			}
		}
		"open" => {
			let req: api::OpenRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let cid = req.cid.clone();
			let thread_id = req.thread_id.clone();
			if thread_id.is_empty() { return Err("thread_id required".into()); }
			if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
			let question = req.question.clone().unwrap_or_else(|| "Continue.".to_string());
			let requested_agent = normalize_agent(req.agent_type.clone());
			let current = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
			if current != requested_agent {
				// append switch_agent step
				let store = crate::qa::session::ThreadStore::new();
				let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
					action: "switch_agent".to_string(),
					args: serde_json::json!({"from": current, "to": requested_agent}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(requested_agent.clone()),
				}).await;
				state.current_agent.insert(thread_id.clone(), requested_agent.clone());
			}
			// ok
			let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
			ok.cid = Some(cid.clone());
			out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
			// processing (retrieving)
			let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.clone());
			pr.for_cid = Some(cid.clone());
			pr.stage = Some(m::processing_response::Stage::Queued);
			pr.progress = Some(0.0);
			let prm = api::ServerMessage::Processing(pr);
			let pr_s = serde_json::to_string(&prm).unwrap();
			state.buffer_last(&pr_s);
			out.push(pr_s);
			// run agent
			let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
			let frames = run_agent_and_frames(&thread_id, &question, &agent).await?;
			for f in frames {
				match f {
					AgentFrame::Final { answer, sql } => {
						// optional token streaming (simple chunking)
						for t in chunk_text(&answer, 24) {
							let mut tk = api::TokenResponse::new(1, m::token_response::Type::Token, now_iso(), state.next_seq(), thread_id.clone(), t);
							tk.for_cid = Some(cid.clone());
							let m = api::ServerMessage::Token(tk);
							let s = serde_json::to_string(&m).unwrap();
							state.buffer_last(&s);
							out.push(s);
						}
						let tseq = state.next_thread_seq(&thread_id);
						let resp = api::ServerMessage::Final(api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.clone(), tseq, api::FinalResponseResult { sql, answer }));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// Log entire thread on final
						{
							let store = crate::qa::session::ThreadStore::new();
							if let Some(log) = store.get(&thread_id).await {
								if let Ok(pretty) = serde_json::to_string_pretty(&log) {
									tracing::info!("{}", pretty);
								}
							}
						}
					}
					AgentFrame::AwaitUser { prompt } => {
						let tseq = state.next_thread_seq(&thread_id);
						let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(1, m::await_user_response::Type::AwaitUser, now_iso(), state.next_seq(), thread_id.clone(), tseq, prompt));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
					}
				}
			}
		}
		"user" => {
			let req: api::UserRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let thread_id = req.thread_id.clone();
			let text = req.text.clone();
			if thread_id.is_empty() || text.trim().is_empty() { return Err("thread_id and text required".into()); }
			if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
				action: "user".to_string(),
				args: serde_json::json!({"text": text}),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some(state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string())),
			}).await;
			// ack
			// we don't increment thread_seq on user ack
			let ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
			out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
		}
		"resume" => {
			let req: api::ResumeRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let after = req.after_seq;
			// Replay frames with seq > after
			for (seq, s) in state.sent.iter() {
				if *seq > after {
					out.push(s.clone());
				}
			}
		}
		"history" => {
			let req: api::HistoryRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let thread_id = req.thread_id.clone();
			if thread_id.is_empty() { return Err("thread_id required".into()); }
			if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
			let (messages, next_before) = build_history(&thread_id, req.before_thread_seq, req.limit).await;
			let mut resp = api::HistoryResponse::new(1, m::history_response::Type::History, now_iso(), state.next_seq(), thread_id.clone(), messages);
			let store = crate::qa::session::ThreadStore::new();
			if let Some(log) = store.get(&thread_id).await { resp.title = log.title; }
			resp.next_before_thread_seq = next_before;
			let outm = api::ServerMessage::History(resp);
			let s = serde_json::to_string(&outm).unwrap();
			state.buffer_last(&s);
			out.push(s);
		}
		"seen" => {
			let req: api::SeenRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let thread_id = req.thread_id.clone();
			state.seen.insert(thread_id.clone(), req.up_to_thread_seq);
			// compute unread now
			let store = crate::qa::session::ThreadStore::new();
			let mut unread = 0;
			if let Some(log) = store.get(&thread_id).await {
				let (_max_assistant, after_seen) = compute_unread_for_log(&log, req.up_to_thread_seq);
				unread = after_seen;
			}
			let resp = api::ServerMessage::Unread(api::UnreadResponse::new(1, m::unread_response::Type::Unread, now_iso(), state.next_seq(), thread_id.clone(), unread));
			let s = serde_json::to_string(&resp).unwrap();
			state.buffer_last(&s);
			out.push(s);
		}
		"delete" => {
			// parse minimal fields without generated model
			let cid = v.get("cid").and_then(|x| x.as_str()).unwrap_or("").to_string();
			let thread_id = v.get("thread_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
			if thread_id.is_empty() { return Err("thread_id required".into()); }
			if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
			// delete thread json
			{
				let store = crate::qa::session::ThreadStore::new();
				let _ = store.delete(&thread_id).await;
			}
			// delete lance embeddings across all pipelines
			{
				let pipelines = crate::sql::registry::list_pipelines().await;
				for p in pipelines {
					let store = crate::qa::vector::lance_store::LanceDbStore::new(&p);
					let _ = store.delete_thread_embeddings(&thread_id).await;
				}
			}
			// respond ok
			let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
			ok.cid = Some(cid);
			out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
		}
		_ => {
			return Err("unknown type".to_string());
		}
	}
	Ok(out)
}

fn now_iso() -> String {
	Utc::now().to_rfc3339()
}

fn truncate_title(s: &str, max_chars: usize) -> String {
	if s.len() <= max_chars { return s.to_string(); }
	match s.char_indices().take_while(|(i, _)| *i < max_chars).last() {
		Some((i, _)) => format!("{}…", &s[..i]),
		None => s.chars().take(max_chars).collect(),
	}
}

async fn synthesize_title(question: &str, answer: &str) -> String {
	// Try LLM to produce a concise title (<= 8 words), else fallback to truncated question
	let cfg = crate::llm::config_from_env();
	let llm = crate::llm::create_llm(&cfg);
	let prompt = format!(
		"Create a very short, descriptive chat title (≤ 8 words).\nRules: plain text only, no quotes, no punctuation beyond spaces, title case.\nQuestion: {}\nAnswer: {}\nTitle:",
		question, answer
	);
	let out = tokio::task::spawn_blocking({ let llm2 = llm.clone(); let p = prompt.clone(); move || llm2.chat(&[crate::llm::ChatMessage { role: "user".into(), content: p }]) }).await;
	if let Ok(Ok(text)) = out {
		let t = text.trim();
		if !t.is_empty() {
			// Normalize whitespace and cap length
			let norm = t.split_whitespace().collect::<Vec<_>>().join(" ");
			return truncate_title(&norm, 64);
		}
	}
	truncate_title(question, 64)
}

struct ConnState {
	seq: i32,
	sent: VecDeque<(i32, String)>,
	thread_seq: HashMap<String, i32>,
	seen: HashMap<String, i32>,
	current_agent: HashMap<String, String>,
}

async fn load_last_run_sql_async(thread_id: &str) -> (Vec<String>, Vec<Vec<String>>) {
	let store = ThreadStore::new();
	if let Some(log) = store.get(thread_id).await {
		for step in log.steps.iter().rev() {
			if step.action == "run_sql" {
				let obs = &step.observation;
				let ok = obs.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
				if ok {
					let header: Vec<String> = obs.get("header").and_then(|h| serde_json::from_value(h.clone()).ok()).unwrap_or_default();
					let rows: Vec<Vec<String>> = obs.get("rows").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or_default();
					if !header.is_empty() && !rows.is_empty() {
						return (header, rows);
					}
				}
			}
		}
	}
	(Vec::new(), Vec::new())
}

async fn synthesize_summary(question: &str, agent_answer: &str, sql_opt: &Option<String>, header: &[String], rows: &[Vec<String>]) -> Option<String> {
	if header.is_empty() || rows.is_empty() {
		return None;
	}
	let cfg = crate::llm::config_from_env();
	let llm = crate::llm::create_llm(&cfg);
	let mut lines: Vec<String> = Vec::new();
	lines.push(format!("Question: {}", question));
	if !agent_answer.trim().is_empty() {
		lines.push(format!("AgentAnswer: {}", agent_answer.trim()));
	}
	if let Some(sql) = sql_opt.as_ref() {
		lines.push(format!("SQL: {}", sql.replace('\n', " ")));
	}
	let max_rows = rows.len().min(24);
	let table_json = serde_json::json!({ "header": header, "rows": &rows[..max_rows] });
	lines.push(format!("Data: {}", table_json.to_string()));
	let prompt = format!(
		"You are a data analyst. Write a short, business-friendly answer for executives. Keep it factual.\n\
		Rules:\n- Be concise (≤ 2 sentences), plain text only.\n- If the data is a time series (date + metric), comment on trend/growth and breadth observed over the period.\n- Do not fabricate numbers; only use provided data.\n- If insufficient data for trends, state the key facts only.\n\nContext:\n{}\n\nAnswer:",
		lines.join("\n")
	);
	match tokio::task::spawn_blocking({ let llm2 = llm.clone(); let p = prompt.clone(); move || llm2.chat(&[crate::llm::ChatMessage { role: "user".into(), content: p }]) }).await {
		Ok(Ok(text)) => {
			let t = text.trim();
			if !t.is_empty() { Some(t.to_string()) } else { None }
		}
		_ => None
	}
}

impl ConnState {
	fn new() -> Self {
		Self { seq: 0, sent: VecDeque::new(), thread_seq: HashMap::new(), seen: HashMap::new(), current_agent: HashMap::new() }
	}
	fn next_seq(&mut self) -> i32 {
		self.seq += 1;
		self.seq
	}
	fn next_thread_seq(&mut self, thread_id: &str) -> i32 {
		let entry = self.thread_seq.entry(thread_id.to_string()).or_insert(0);
		*entry += 1;
		*entry
	}
	fn buffer_last(&mut self, json: &str) {
		// try to extract seq
		if let Ok(v) = serde_json::from_str::<Value>(json) {
			if let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) {
				self.sent.push_back((seq as i32, json.to_string()));
				while self.sent.len() > 500 {
					self.sent.pop_front();
				}
			}
		}
	}
}

async fn process_new(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let req: api::NewRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
	let cid = req.cid.clone();
	let question = req.question.clone();
	if question.trim().is_empty() { return Err("question required".into()); }
	let thread_id = Uuid::new_v4().to_string();
	let agent = normalize_agent(req.agent_type.clone());
	state.current_agent.insert(thread_id.clone(), agent.clone());
	// ok
	let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
	ok.cid = Some(cid.clone());
	{
		let s = serde_json::to_string(&ok).unwrap();
		tracing::info!("WS -> {}", s);
		let _ = write.send(Message::Text(s)).await;
	}
	// thread_assigned
	let ta = api::ThreadAssignedResponse::new(1, m::thread_assigned_response::Type::ThreadAssigned, now_iso(), state.next_seq(), cid.clone(), thread_id.clone());
	{
		let s = serde_json::to_string(&ta).unwrap();
		state.buffer_last(&s);
		tracing::info!("WS -> {}", s);
		let _ = write.send(Message::Text(s)).await;
	}
	// processing
	let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.clone());
	pr.for_cid = Some(cid.clone());
	pr.stage = Some(m::processing_response::Stage::Queued);
	pr.progress = Some(0.0);
	{
		let s = serde_json::to_string(&pr).unwrap();
		state.buffer_last(&s);
		tracing::info!("WS -> {}", s);
		let _ = write.send(Message::Text(s)).await;
	}
	// agent with periodic updates
	// record initial user question in thread history
	{
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": question}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
		let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
	}
	run_agent_with_processing(&thread_id, &question, &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}

async fn process_open(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let req: api::OpenRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
	let cid = req.cid.clone();
	let thread_id = req.thread_id.clone();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	let question = req.question.clone().unwrap_or_else(|| "Continue.".to_string());
	let requested_agent = normalize_agent(req.agent_type.clone());
	let current = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
	if current != requested_agent {
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
			action: "switch_agent".to_string(),
			args: serde_json::json!({"from": current, "to": requested_agent}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(requested_agent.clone()),
		}).await;
		state.current_agent.insert(thread_id.clone(), requested_agent.clone());
	}
	// ok
	let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
	ok.cid = Some(cid.clone());
	{
		let s = serde_json::to_string(&ok).unwrap();
		tracing::info!("WS -> {}", s);
		let _ = write.send(Message::Text(s)).await;
	}
	// processing
	let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.clone());
	pr.for_cid = Some(cid.clone());
	pr.stage = Some(m::processing_response::Stage::Queued);
	pr.progress = Some(0.0);
	{
		let s = serde_json::to_string(&pr).unwrap();
		state.buffer_last(&s);
		tracing::info!("WS -> {}", s);
		let _ = write.send(Message::Text(s)).await;
	}
	// agent with periodic updates
	let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
	// if user supplied a prompt on open, record it
	if !question.trim().is_empty() {
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": question}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
		let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
	}
	run_agent_with_processing(&thread_id, &question, &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}

fn normalize_agent(a: Option<String>) -> String {
	match a.unwrap_or_else(|| "ask".to_string()).to_lowercase().as_str() {
		"cleanse" => "cleanse".to_string(),
		"model" => "model".to_string(),
		_ => "ask".to_string(),
	}
}

async fn run_agent_with_processing(
	thread_id: &str,
	question: &str,
	agent: &str,
	cid: &str,
	state: &mut ConnState,
	write: &mut (impl SinkExt<Message> + Unpin),
	initial_stage: m::processing_response::Stage,
) -> Result<(), String> {
	// Prepare agent
	let mut sys = crate::qa::prompts::system_prompt();
	let now_utc = chrono::Utc::now().to_rfc3339();
	let now_local = chrono::Local::now();
	let local_iso = now_local.to_rfc3339();
	let local_offset = now_local.offset().to_string();
	sys = format!(
		"{}\n\nTimeContext:\n- NowUTC: {}\n- UserLocal: {} (offset {})",
		sys, now_utc, local_iso, local_offset
	);
	let tools_card = crate::qa::prompts::tool_card();
	let ctx_df = datafusion::prelude::SessionContext::new();
	crate::ws::agent_runner::pre_register_all_namespaces(&ctx_df).await;
	let mut registry = crate::ws::agent_runner::build_registry(agent, &ctx_df);
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
	let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
	let actx = AgentCtx {
		pipeline: "".to_string(),
		namespace: None,
		top_k: 30,
		per_step_timeout_secs: 10,
		max_steps: 10,
		thread_id: Some(thread_id.to_string()),
		progress_tx: Some(tx),
		pre_step_tx: Some(pre_tx),
		agent_name: Some(agent.to_string()),
	};
	let question2 = crate::ws::agent_runner::inject_agent_question(agent, question);
	let mut fut = Box::pin(Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2));
	let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
	let mut last_step_completed: usize = 0;
	let max_steps = actx.max_steps;
	loop {
		tokio::select! {
			Some(step_name) = pre_rx.recv() => {
				// Emit processing BEFORE executing the step
				let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.to_string());
				pr.for_cid = Some(cid.to_string());
				// Map to new coarse stage enum
				pr.stage = Some(m::processing_response::Stage::Processing);
				// Progress: last completed / max, unchanged here
				let progress = if last_step_completed == 0 { 0.0 } else { (last_step_completed as f64) / (max_steps as f64) };
				pr.progress = Some(progress);
				// Only emit known non-empty step names
				match step_name.as_str() {
					"run_sql" | "sql_schema" | "sql_stats" | "sql_sample" | "vect_query" => { pr.step = Some(step_name); }
					_ => {}
				}
				let s = serde_json::to_string(&pr).unwrap();
				state.buffer_last(&s);
				tracing::info!("WS -> {}", s);
				let _ = write.send(Message::Text(s)).await;
			}
			Some(step_idx) = rx.recv() => {
				last_step_completed = step_idx;
				let progress = (last_step_completed as f64) / (max_steps as f64);
				let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.to_string());
				pr.for_cid = Some(cid.to_string());
				pr.stage = Some(m::processing_response::Stage::Processing);
				pr.progress = Some(progress);
				pr.step = None;
				let s = serde_json::to_string(&pr).unwrap();
				state.buffer_last(&s);
				tracing::info!("WS -> {}", s);
				let _ = write.send(Message::Text(s)).await;
			}
			res = &mut fut => {
				match res {
					Ok(RunOutcome::Final { thread_id: _tid, result }) => {
						// Emit terminal processing frame (complete)
						{
							let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.to_string());
							pr.for_cid = Some(cid.to_string());
							pr.stage = Some(m::processing_response::Stage::Complete);
							pr.progress = Some(1.0);
							let s = serde_json::to_string(&pr).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;
						}
						// Try to synthesize a richer summary using last run_sql data
						let (header, rows) = load_last_run_sql_async(thread_id).await;
						let improved = synthesize_summary(question, &result.answer, &result.sql, &header, &rows).await;
						let streamed_answer = improved.as_deref().unwrap_or(&result.answer);
						// Finalize title once
						{
							let store = crate::qa::session::ThreadStore::new();
							let title = synthesize_title(question, streamed_answer).await;
							let _ = store.finalize_title(thread_id, &title).await;
						}
						for t in chunk_text(streamed_answer, 24) {
							let mut tk = api::TokenResponse::new(1, m::token_response::Type::Token, now_iso(), state.next_seq(), thread_id.to_string(), t);
							tk.for_cid = Some(cid.to_string());
							let s = serde_json::to_string(&tk).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;
						}
						let tseq = state.next_thread_seq(thread_id);
						let final_answer = improved.unwrap_or(result.answer.clone());
						let resp = api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.to_string(), tseq, api::FinalResponseResult { sql: result.sql, answer: final_answer });
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						tracing::info!("WS -> {}", s);
						let _ = write.send(Message::Text(s)).await;
						// Log entire thread on final
						{
							let store = crate::qa::session::ThreadStore::new();
							if let Some(log) = store.get(thread_id).await {
								if let Ok(pretty) = serde_json::to_string_pretty(&log) {
									tracing::info!("{}", pretty);
								}
							}
						}
						return Ok(());
					}
					Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => {
						let tseq = state.next_thread_seq(thread_id);
						let resp = api::AwaitUserResponse::new(1, m::await_user_response::Type::AwaitUser, now_iso(), state.next_seq(), thread_id.to_string(), tseq, prompt);
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						tracing::info!("WS -> {}", s);
						let _ = write.send(Message::Text(s)).await;
						return Ok(());
					}
					Err(e) => {
						// Emit terminal processing frame (error)
						{
							let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.to_string());
							pr.for_cid = Some(cid.to_string());
							pr.stage = Some(m::processing_response::Stage::Error);
							let s = serde_json::to_string(&pr).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;
						}
						return Err(e.to_string());
					}
				}
			}
			_ = ticker.tick() => {
				// periodic processing update: re-emit last known progress to keep clients informed
				if last_step_completed > 0 {
					let progress = (last_step_completed as f64) / (max_steps as f64);
					let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.to_string());
					pr.for_cid = Some(cid.to_string());
					pr.stage = Some(initial_stage.clone());
					pr.progress = Some(progress);
					let s = serde_json::to_string(&pr).unwrap();
					state.buffer_last(&s);
					tracing::info!("WS -> {}", s);
					let _ = write.send(Message::Text(s)).await;
				}
			}
		}
	}
}

enum AgentFrame {
	Final { answer: String, sql: Option<String> },
	AwaitUser { prompt: String },
}

async fn run_agent_and_frames(thread_id: &str, question: &str, agent: &str) -> Result<Vec<AgentFrame>, String> {
	let mut sys = crate::qa::prompts::system_prompt();
	// Inject TimeContext
	let now_utc = chrono::Utc::now().to_rfc3339();
	let now_local = chrono::Local::now();
	let local_iso = now_local.to_rfc3339();
	let local_offset = now_local.offset().to_string();
	sys = format!(
		"{}\n\nTimeContext:\n- NowUTC: {}\n- UserLocal: {} (offset {})",
		sys, now_utc, local_iso, local_offset
	);
	let tools_card = crate::qa::prompts::tool_card();
	let mut registry = ToolRegistry::new();
	let ctx_df = datafusion::prelude::SessionContext::new();
	{
		// Pre-register all pipelines' namespaces so two-part names work regardless of active pipeline
		let pipelines = crate::sql::registry::list_pipelines().await;
		for pipeline in pipelines {
			let mut namespaces = crate::sql::registry::list_namespaces(&pipeline).await;
			namespaces.sort();
			for ns in namespaces {
				let _ = crate::sql::tables::register_namespace_view(&ctx_df, &pipeline, &ns).await;
			}
			let _ = crate::sql::tables::register_deadletters(&ctx_df, &pipeline).await;
		}
	}
	match agent {
		"cleanse" => {
			registry.register(SqlRunTool { ctx: ctx_df.clone() });
			registry.register(SqlSchemaTool { ctx: ctx_df.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx_df.clone() });
			registry.register(VectQueryTool);
			registry.register(AskUserTool);
		}
		"model" => {
			registry.register(SqlRunTool { ctx: ctx_df.clone() });
			registry.register(SqlSchemaTool { ctx: ctx_df.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx_df.clone() });
			registry.register(VectQueryTool);
			registry.register(AskUserTool);
		}
		_ => {
			registry.register(SqlRunTool { ctx: ctx_df.clone() });
			registry.register(SqlSchemaTool { ctx: ctx_df.clone() });
			registry.register(SqlStatsTool);
			registry.register(SqlSampleTool { ctx: ctx_df.clone() });
			registry.register(VectQueryTool);
			registry.register(AskUserTool);
		}
	}
	let actx = AgentCtx { pipeline: "".to_string(), namespace: None, top_k: 30, per_step_timeout_secs: 10, max_steps: 10, thread_id: Some(thread_id.to_string()), progress_tx: None, pre_step_tx: None, agent_name: Some(agent.to_string()) };
	let mut frames: Vec<AgentFrame> = Vec::new();
	let question2 = crate::ws::agent_runner::inject_agent_question(agent, question);
	match Agent::run_until_block(&registry, &actx, &sys, &tools_card, &question2).await {
		Ok(RunOutcome::Final { thread_id: _tid, result }) => {
			let (header, rows) = load_last_run_sql_async(thread_id).await;
			let improved = synthesize_summary(&question2, &result.answer, &result.sql, &header, &rows).await;
			let answer = improved.unwrap_or(result.answer);
			frames.push(AgentFrame::Final { answer, sql: result.sql });
		}
		Ok(RunOutcome::AwaitUser { thread_id: _tid, prompt }) => {
			frames.push(AgentFrame::AwaitUser { prompt });
		}
		Err(e) => return Err(e),
	}
	Ok(frames)
}

fn chunk_text(s: &str, max_chunk: usize) -> Vec<String> {
	if s.is_empty() { return Vec::new(); }
	let mut out: Vec<String> = Vec::new();
	let mut buf = String::new();
	for w in s.split_whitespace() {
		if buf.is_empty() {
			buf.push_str(w);
		} else if buf.len() + 1 + w.len() <= max_chunk {
			buf.push(' ');
			buf.push_str(w);
		} else {
			out.push(buf.clone());
			buf.clear();
			buf.push_str(w);
		}
	}
	if !buf.is_empty() { out.push(buf); }
	out
}

fn compute_unread_for_log(log: &crate::qa::session::ThreadLog, seen_seq: i32) -> (i32, i32) {
	let mut tseq: i32 = 0;
	let mut assistant_count_after_seen: i32 = 0;
	let mut last_assistant_seq: i32 = 0;
	for step in log.steps.iter() {
		match step.action.as_str() {
			"user" => {
				tseq += 1;
			}
			"final" | "ask_user" => {
				tseq += 1;
				if tseq > seen_seq {
					assistant_count_after_seen += 1;
				}
				last_assistant_seq = tseq;
			}
			_ => {}
		}
	}
	(last_assistant_seq, assistant_count_after_seen)
}

async fn build_history(thread_id: &str, before: Option<i32>, limit_opt: Option<i32>) -> (Vec<api::HistoryResponseMessagesInner>, Option<i32>) {
	let store = crate::qa::session::ThreadStore::new();
	let mut msgs: Vec<api::HistoryResponseMessagesInner> = Vec::new();
	let mut next_before: Option<i32> = None;
	let limit = limit_opt.unwrap_or(50).max(1);
	if let Some(log) = store.get(thread_id).await {
		let mut tseq: i32 = 0;
		let mut all_msgs: Vec<api::HistoryResponseMessagesInner> = Vec::new();
		for step in log.steps.iter() {
			match step.action.as_str() {
				"user" => {
					tseq += 1;
					let content = step.args.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
					all_msgs.push(api::HistoryResponseMessagesInner {
						thread_seq: tseq,
						role: m::history_response_messages_inner::Role::User,
						content,
						created_at: step.ts.clone(),
					});
				}
				"final" => {
					tseq += 1;
					let content = step.args.get("answer").and_then(|x| x.as_str()).unwrap_or("").to_string();
					all_msgs.push(api::HistoryResponseMessagesInner {
						thread_seq: tseq,
						role: m::history_response_messages_inner::Role::Assistant,
						content,
						created_at: step.ts.clone(),
					});
				}
				"ask_user" => {
					tseq += 1;
					let content = step.observation.get("prompt").and_then(|x| x.as_str()).unwrap_or("").to_string();
					all_msgs.push(api::HistoryResponseMessagesInner {
						thread_seq: tseq,
						role: m::history_response_messages_inner::Role::Assistant,
						content,
						created_at: step.ts.clone(),
					});
				}
				_ => {}
			}
		}
		// apply before and limit
		let mut filtered: Vec<api::HistoryResponseMessagesInner> = if let Some(b) = before {
			all_msgs.into_iter().filter(|m| m.thread_seq < b).collect()
		} else {
			all_msgs
		};
		let total = filtered.len() as i32;
		if total > limit {
			let start = (total - limit) as usize;
			let trimmed = filtered.split_off(start);
			let first_seq = trimmed.first().map(|m| m.thread_seq).unwrap_or(0);
			next_before = if first_seq > 1 { Some(first_seq) } else { None };
			msgs = trimmed;
		} else {
			msgs = filtered;
			next_before = None;
		}
	}
	(msgs, next_before)
}

