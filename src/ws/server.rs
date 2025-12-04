use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use crate::qa::agent::{Agent, AgentCtx, RunOutcome};
// Removed unused tool imports; flows handle registry/tool selection
use uuid::Uuid;
use crate::ws::api_gen as api;
use std::collections::{HashMap, VecDeque};
use chrono::Utc;
use crate::models as m;
use crate::qa::session::ThreadStore;

// Steering prompts removed for model agent; model runs eagerly without awaiting user choice.

pub async fn start(port: u16) -> Result<(), String> {
    // Ensure configuration is loaded so tenant/workspace are correct for this process
    crate::helpers::configuration::Config::build_config();
    // Kick off global DBT examples sync (non-blocking)
    tokio::spawn(async {
        crate::qa::dbt_examples::ensure_synced_once().await;
    });
    // Bootstrap pre-registration of all namespaces once on startup (background)
    tokio::spawn(async {
        let ctx0 = datafusion::prelude::SessionContext::new();
        crate::ws::agent_runner::pre_register_all_namespaces(&ctx0).await;
    });
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
									} else if t == "approve" {
										if let Err(e) = process_approve(&v, &mut state, &mut write).await {
											let cid_guess = v.get("cid").and_then(|x| x.as_str()).map(|s| s.to_string());
											let mut err = api::ErrorResponse::new(1, m::error_response::Type::Error, now_iso(), e.clone());
											err.code = Some("invalid_request".to_string());
											err.cid = cid_guess;
											let s = serde_json::to_string(&err).unwrap_or_else(|_| "{\"v\":1,\"type\":\"error\",\"server_time\":\"\",\"error\":\"internal\"}".to_string());
											tracing::info!("WS -> {}", s);
											let _ = write.send(Message::Text(s)).await;
										}
										continue;
									} else if t == "reject" {
										if let Err(e) = process_reject(&v, &mut state, &mut write).await {
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
			let agent = normalize_agent_new(req.agent_type.clone());
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
						let sql = if agent == "model" { None } else { sql };
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
						let resp = api::ServerMessage::Final(api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.clone(), tseq, api::FinalResponseResult { sql: sql.clone(), answer: answer.clone(), data: None, chart: None }));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// Append final_response step with the exact payload sent
						{
							let store = crate::qa::session::ThreadStore::new();
							let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
								action: "final_response".to_string(),
								args: serde_json::json!({"answer": answer, "sql": sql, "data": null, "chart": null}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
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
						let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(1, m::await_user_response::Type::AwaitUser, now_iso(), state.next_seq(), thread_id.clone(), tseq, prompt.clone()));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// persist gate in thread
						{
							let store = crate::qa::session::ThreadStore::new();
							let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
								action: "await_user".to_string(),
								args: serde_json::json!({"prompt": prompt}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
					}
					AgentFrame::AwaitApproval { prompt } => {
						let tseq = state.next_thread_seq(&thread_id);
						let outv = serde_json::json!({
							"v": 1,
							"type": "await_approval",
							"server_time": now_iso(),
							"seq": state.next_seq(),
							"thread_id": thread_id.clone(),
							"thread_seq": tseq,
							"prompt": prompt,
						});
						let s = outv.to_string();
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
			let requested_agent = normalize_agent_open(req.agent_type.clone());
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
						let sql = if agent == "model" { None } else { sql };
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
						let resp = api::ServerMessage::Final(api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.clone(), tseq, api::FinalResponseResult { sql: sql.clone(), answer: answer.clone(), data: None, chart: None }));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// Append final_response step with the exact payload sent
						{
							let store = crate::qa::session::ThreadStore::new();
							let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
								action: "final_response".to_string(),
								args: serde_json::json!({"answer": answer, "sql": sql, "data": null, "chart": null}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
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
						let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(1, m::await_user_response::Type::AwaitUser, now_iso(), state.next_seq(), thread_id.clone(), tseq, prompt.clone()));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// persist gate in thread
						{
							let store = crate::qa::session::ThreadStore::new();
							let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
								action: "await_user".to_string(),
								args: serde_json::json!({"prompt": prompt}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
					}
					AgentFrame::AwaitApproval { prompt } => {
						let tseq = state.next_thread_seq(&thread_id);
						let outv = serde_json::json!({
							"v": 1,
							"type": "await_approval",
							"server_time": now_iso(),
							"seq": state.next_seq(),
							"thread_id": thread_id.clone(),
							"thread_seq": tseq,
							"prompt": prompt,
						});
						let s = outv.to_string();
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
			// Steering gates removed: user messages no longer drive model/metric type or existing/new choices.
			// auto-resume: emit processing and continue agent immediately
			let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
			// processing
			let mut pr = api::ProcessingResponse::new(1, m::processing_response::Type::Processing, now_iso(), state.next_seq(), thread_id.clone());
			pr.stage = Some(m::processing_response::Stage::Queued);
			pr.progress = Some(0.0);
			let prm = api::ServerMessage::Processing(pr);
			let pr_s = serde_json::to_string(&prm).unwrap();
			state.buffer_last(&pr_s);
			out.push(pr_s);
			// run agent for this thread
			tracing::info!("user auto-resume: thread_id={} agent={}", thread_id, agent);
			// Use the first user question to preserve embeddings context
			let store2 = crate::qa::session::ThreadStore::new();
			let mut q_for_resume = "Continue.".to_string();
			if let Some(log) = store2.get(&thread_id).await {
				if let Some(first_user) = log.steps.iter().find(|s| s.action == "user") {
					if let Some(t) = first_user.args.get("text").and_then(|x| x.as_str()) {
						if !t.trim().is_empty() { q_for_resume = t.to_string(); }
					}
				}
			}
			let frames = run_agent_and_frames(&thread_id, &q_for_resume, &agent).await?;
			for f in frames {
				match f {
					AgentFrame::Final { answer, sql } => {
						// optional token streaming (simple chunking); no cid here in generic handler
						for t in chunk_text(&answer, 24) {
							let mut tk = api::TokenResponse::new(1, m::token_response::Type::Token, now_iso(), state.next_seq(), thread_id.clone(), t);
							let m = api::ServerMessage::Token(tk);
							let s = serde_json::to_string(&m).unwrap();
							state.buffer_last(&s);
							out.push(s);
						}
						let tseq = state.next_thread_seq(&thread_id);
						let resp = api::ServerMessage::Final(api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.clone(), tseq, api::FinalResponseResult { sql: sql.clone(), answer: answer.clone(), data: None, chart: None }));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						// Append final_response step with the exact payload sent
						{
							let store = crate::qa::session::ThreadStore::new();
							let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
								action: "final_response".to_string(),
								args: serde_json::json!({"answer": answer, "sql": sql, "data": null, "chart": null}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string())),
							}).await;
						}
						// Log entire thread on final (best-effort)
						if let Some(log) = store.get(&thread_id).await {
							if let Ok(pretty) = serde_json::to_string_pretty(&log) {
								tracing::info!("{}", pretty);
							}
						}
					}
					AgentFrame::AwaitUser { prompt } => {
						let tseq = state.next_thread_seq(&thread_id);
						let resp = api::ServerMessage::AwaitUser(api::AwaitUserResponse::new(1, m::await_user_response::Type::AwaitUser, now_iso(), state.next_seq(), thread_id.clone(), tseq, prompt.clone()));
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
					}
					AgentFrame::AwaitApproval { prompt } => {
						let tseq = state.next_thread_seq(&thread_id);
						let outv = serde_json::json!({
							"v": 1,
							"type": "await_approval",
							"server_time": now_iso(),
							"seq": state.next_seq(),
							"thread_id": thread_id.clone(),
							"thread_seq": tseq,
							"prompt": prompt,
						});
						let s = outv.to_string();
						state.buffer_last(&s);
						out.push(s);
					}
				}
			}
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
	let agent = normalize_agent_new(req.agent_type.clone());
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
	let requested_agent = normalize_agent_open(req.agent_type.clone());
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

async fn process_approve(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let cid = v.get("cid").and_then(|x| x.as_str()).ok_or_else(|| "cid required".to_string())?.to_string();
	let thread_id = v.get("thread_id").and_then(|x| x.as_str()).ok_or_else(|| "thread_id required".to_string())?.to_string();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
	// ack
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
	// append user=approve step
	{
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": "approve"}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
	}
	tracing::info!("approve: thread_id={} agent={}", thread_id, agent);
	run_agent_with_processing(&thread_id, "Continue.", &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}

async fn process_reject(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let cid = v.get("cid").and_then(|x| x.as_str()).ok_or_else(|| "cid required".to_string())?.to_string();
	let thread_id = v.get("thread_id").and_then(|x| x.as_str()).ok_or_else(|| "thread_id required".to_string())?.to_string();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string());
	// ack
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
	// append user=reject step
	{
		let store = crate::qa::session::ThreadStore::new();
		let _ = store.append_step(&thread_id, crate::qa::session::ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": "reject"}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
	}
	tracing::info!("reject: thread_id={} agent={}", thread_id, agent);
	run_agent_with_processing(&thread_id, "Continue.", &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}
// normalize_agent removed (unused)

fn normalize_agent_new(a: Option<api::new_request::AgentType>) -> String {
	match a {
		Some(api::new_request::AgentType::Cleanse) => "cleanse".to_string(),
		Some(api::new_request::AgentType::Model) => "model".to_string(),
		_ => "ask".to_string(),
	}
}

fn normalize_agent_open(a: Option<api::open_request::AgentType>) -> String {
	match a {
		Some(api::open_request::AgentType::Cleanse) => "cleanse".to_string(),
		Some(api::open_request::AgentType::Model) => "model".to_string(),
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
	let mut sys = if agent == "model" { crate::qa::prompts_model::model_system_prompt() } else { crate::qa::prompts::system_prompt() };
	let now_utc = chrono::Utc::now().to_rfc3339();
	let now_local = chrono::Local::now();
	let local_iso = now_local.to_rfc3339();
	let local_offset = now_local.offset().to_string();
	sys = format!(
		"{}\n\nTimeContext:\n- NowUTC: {}\n- UserLocal: {} (offset {})",
		sys, now_utc, local_iso, local_offset
	);
	let tools_card = if agent == "model" { crate::qa::prompts_model::model_tool_card() } else { crate::qa::prompts::tool_card() };
	// Use the existing thread-scoped SessionContext so registrations persist across steps for this thread
	let ctx_df = crate::ws::agent_runner::get_or_create_thread_ctx(thread_id);
	// Also register compiled dbt models as dbt.<model> views if present
	let _ = crate::sql::tables::register_dbt_models(&ctx_df).await;
	// If dbt models are registered, surface them to the LLM; else note unavailability
	{
		use datafusion::catalog::CatalogProvider;
		let state_df = ctx_df.state();
		let cat_list = state_df.catalog_list();
		if let Some(cat) = cat_list.catalog("datafusion") {
			if let Some(schema) = cat.schema("dbt") {
				let names = schema.table_names();
				if names.is_empty() {
					tracing::info!("DBT models: 0 compiled views available");
					let store = crate::qa::session::ThreadStore::new();
					let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
						action: "dbt_models_unavailable".to_string(),
						args: serde_json::json!({"notice":"No dbt.<model> views available; falling back to raw datasets for this question"}),
						observation: serde_json::json!({"ok": true}),
						ts: chrono::Utc::now().to_rfc3339(),
						agent: Some(agent.to_string()),
					}).await;
				} else {
					tracing::info!("DBT models: {} compiled view(s) available", names.len());
					let store = crate::qa::session::ThreadStore::new();
					let items: Vec<serde_json::Value> = names.into_iter().take(50).map(|n| {
						serde_json::json!({"pipeline": "", "namespace": "", "name": n, "kind": "model", "score": 1.0})
					}).collect();
					let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
						action: "resolved_artifacts".to_string(),
						args: serde_json::json!({"items": items}),
						observation: serde_json::json!({"ok": true}),
						ts: chrono::Utc::now().to_rfc3339(),
						agent: Some(agent.to_string()),
					}).await;
				}
			}
		}
	}
	let registry = crate::ws::agent_runner::build_registry(agent, &ctx_df);
	// Determine question for embeddings (prefer first user text)
	let q_for_embed = {
		let storeq = crate::qa::session::ThreadStore::new();
		let mut q = question.to_string();
		if let Some(log) = storeq.get(thread_id).await {
			if let Some(first_user) = log.steps.iter().find(|s| s.action == "user") {
				if let Some(t) = first_user.args.get("text").and_then(|x| x.as_str()) {
					if !t.trim().is_empty() { q = t.to_string(); }
				}
			}
		}
		q
	};
	// Preflight: resolve dataset candidates for all agents (broader K) with per-thread cache
	let candidates = {
		let ttl_secs: u64 = 600;
		let cache_hit = crate::qa::session::ThreadCacheStore::get(thread_id)
			.and_then(|c| if !c.candidates.is_empty() && c.ttl_fresh(ttl_secs) { Some(c.candidates) } else { None });
        if let Some(cands) = cache_hit {
			tracing::debug!("cache:candidates hit (thread_id={})", thread_id);
            cands.into_iter().map(|(p,n,s)| crate::ws::context::DatasetResolved { pipeline: p, namespace: n, score: s, fields_hint: String::new() }).collect::<Vec<_>>()
		} else {
			tracing::debug!("cache:candidates miss (thread_id={})", thread_id);
			let cands = crate::ws::context::resolve_datasets(&q_for_embed, 50).await;
			let pairs = cands.iter().map(|c| (c.pipeline.clone(), c.namespace.clone(), c.score)).collect::<Vec<_>>();
			crate::qa::session::ThreadCacheStore::update_candidates(thread_id, pairs);
			cands
		}
	};
	// Register only selected namespaces for the current question (no bulk registration), on this same thread context
	if !candidates.is_empty() {
		let mut pairs: Vec<(String, String)> = Vec::new();
		for c in &candidates {
			pairs.push((c.pipeline.clone(), c.namespace.clone()));
		}
		crate::ws::agent_runner::pre_register_selected_namespaces(&ctx_df, &pairs).await;
	}
	// For the top few candidates, compact schema and sample a few rows; update thread cache and add a schema_context step
	{
		use datafusion::catalog::CatalogProvider;
		let top = candidates.iter().take(5);
		let mut tables_ctx: Vec<serde_json::Value> = Vec::new();
		for c in top {
			let fqn = format!("{}.{}", c.pipeline, c.namespace);
			// try to load table and read schema
			if let Ok(df) = ctx_df.table(&fqn).await {
				let schema = df.schema();
				let cols = compact_schema_columns(schema.fields());
				crate::qa::session::ThreadCacheStore::update_schema(thread_id, &fqn, cols.clone());
				// sample rows
				let sample_sql = format!("SELECT * FROM {} LIMIT 3", fqn);
				let mut rows_out: Vec<Vec<String>> = Vec::new();
				if let Ok(df2) = ctx_df.sql(&sample_sql).await {
					if let Ok(batches) = df2.collect().await {
						for b in batches.iter() {
							for r in 0..b.num_rows() {
								let mut row: Vec<String> = Vec::new();
								for cidx in 0..b.num_columns() {
									row.push(crate::sql::tui::value_to_string(b.column(cidx).as_ref(), r));
								}
								rows_out.push(row);
								if rows_out.len() >= 3 { break; }
							}
							if rows_out.len() >= 3 { break; }
						}
					}
				}
				if !rows_out.is_empty() {
					crate::qa::session::ThreadCacheStore::update_samples(thread_id, &fqn, rows_out.clone());
				}
				tables_ctx.push(serde_json::json!({
					"dataset": fqn,
					"columns": cols.iter().map(|(n,t)| serde_json::json!({"name": n, "type": t})).collect::<Vec<_>>(),
					"samples": rows_out
				}));
			}
		}
		if !tables_ctx.is_empty() {
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
				action: "schema_context".to_string(),
				args: serde_json::json!({"tables": tables_ctx}),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some(agent.to_string()),
			}).await;
		}
	}
	if !candidates.is_empty() {
		// Append thread step
		let store = crate::qa::session::ThreadStore::new();
		let arr: Vec<serde_json::Value> = candidates.iter().map(|c| serde_json::json!({"pipeline": c.pipeline, "namespace": c.namespace, "score": c.score})).collect();
		let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
			action: "resolved_datasets".to_string(),
			args: serde_json::json!({"candidates": arr}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.to_string()),
		}).await;
	}
	// Preflight: resolve metric artifacts and append step for all agents
	{
		let arts = crate::ws::context::resolve_artifacts(&q_for_embed, 3, "metric").await;
		if !arts.is_empty() {
			let store = crate::qa::session::ThreadStore::new();
			let arr: Vec<serde_json::Value> = arts.iter().map(|a| serde_json::json!({
				"pipeline": a.pipeline, "namespace": a.namespace, "name": a.name, "kind": a.kind, "score": a.score
			})).collect();
			let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
				action: "resolved_artifacts".to_string(),
				args: serde_json::json!({"items": arr}),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some(agent.to_string()),
			}).await;
		}
	}
	// Check for existing confident selection to avoid re-gating
	let mut have_selection = false;
	{
		let store = crate::qa::session::ThreadStore::new();
		if let Some(log) = store.get(thread_id).await {
			for step in log.steps.iter().rev() {
				if step.action == "preflight_decision" {
					if let (Some(sel), Some(conf)) = (step.args.get("selection"), step.args.get("confidence").and_then(|v| v.as_f64())) {
						if sel.is_object() && conf >= 0.5 { have_selection = true; }
					}
					break;
				}
			}
		}
	}
	// Preflight: think-out-loud intent and decision; gate if low confidence
	let mut decision = crate::ws::context::PreflightDecision::default();
	if !have_selection {
		let intent = crate::ws::context::preflight_intent_llm(&q_for_embed).await;
		{
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
				action: "preflight_intent".to_string(),
				args: serde_json::to_value(&intent).unwrap_or(serde_json::json!({})),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some(agent.to_string()),
			}).await;
		}
		let arts_vec = {
			let store = crate::qa::session::ThreadStore::new();
			let mut out: Vec<crate::ws::context::ResolvedArtifact> = Vec::new();
			if let Some(log) = store.get(thread_id).await {
				for step in log.steps.iter().rev() {
					if step.action == "resolved_artifacts" {
						if let Some(arr) = step.args.get("items").and_then(|x| x.as_array()) {
							for v in arr {
								let p = v.get("pipeline").and_then(|x| x.as_str()).unwrap_or("").to_string();
								let ns = v.get("namespace").and_then(|x| x.as_str()).unwrap_or("").to_string();
								let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
								let score = v.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
								out.push(crate::ws::context::ResolvedArtifact { pipeline: p, namespace: ns, name, kind: "metric".to_string(), score, text: String::new() });
							}
						}
						break;
					}
				}
			}
			out
		};
		// Reuse last confident decision if exists, otherwise ask LLM
		decision = {
			let store = crate::qa::session::ThreadStore::new();
			let mut out: Option<crate::ws::context::PreflightDecision> = None;
			if let Some(log) = store.get(thread_id).await {
				for step in log.steps.iter().rev() {
					if step.action == "preflight_decision" {
						if let Ok(d) = serde_json::from_value::<crate::ws::context::PreflightDecision>(step.args.clone()) {
							if d.selection.is_some() && d.confidence >= 0.5 { out = Some(d); }
						}
						break;
					}
				}
			}
			if let Some(d) = out {
				d
			} else {
				crate::ws::context::preflight_decision_llm(&intent, &candidates, &arts_vec).await
			}
		};
		// Append decision (best-effort, may be similar to last)
		{
			let store = crate::qa::session::ThreadStore::new();
			let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
				action: "preflight_decision".to_string(),
				args: serde_json::to_value(&decision).unwrap_or(serde_json::json!({})),
				observation: serde_json::json!({"ok": true}),
				ts: chrono::Utc::now().to_rfc3339(),
				agent: Some(agent.to_string()),
			}).await;
		}
		// Gates based on decision
		// Stage A removed: do not gate on DBT type; proceed eagerly.
		// Stage B removed: do not gate on existing/new; proceed eagerly.
	}
	// When a confident selection already exists, load it into `decision`
	if have_selection {
		let store = crate::qa::session::ThreadStore::new();
		if let Some(log) = store.get(thread_id).await {
			for step in log.steps.iter().rev() {
				if step.action == "preflight_decision" {
					if let Ok(d) = serde_json::from_value::<crate::ws::context::PreflightDecision>(step.args.clone()) {
						if d.selection.is_some() && d.confidence >= 0.5 {
							decision = d;
						}
					}
					break;
				}
			}
		}
	}
	// Removed static reference example injection for model agent.
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
	let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
	let actx = AgentCtx {
		top_k: 100,
		per_step_timeout_secs: 10,
		max_steps: 10,
		thread_id: Some(thread_id.to_string()),
		progress_tx: Some(tx),
		pre_step_tx: Some(pre_tx),
		agent_name: Some(agent.to_string()),
		dataset_candidates: candidates.iter().map(|c| crate::qa::agent::DatasetCandidate { pipeline: c.pipeline.clone(), namespace: c.namespace.clone(), score: c.score }).collect(),
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
					"run_sql" => { pr.step = Some(m::processing_response::Step::RunSql); }
					"sql_schema" => { pr.step = Some(m::processing_response::Step::SqlSchema); }
					"sql_stats" => { pr.step = Some(m::processing_response::Step::SqlStats); }
					"sql_sample" => { pr.step = Some(m::processing_response::Step::SqlSample); }
					"vect_query" => { pr.step = Some(m::processing_response::Step::VectQuery); }
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
						// Suggest a simple chart based on data shape (ask agent only)
						let mut chart: Option<api::FinalResponseResultChart> = None;
						if agent == "ask" && !header.is_empty() && !rows.is_empty() {
							// Heuristic: if first col looks like time and there is at least one numeric column -> line chart
							let x_col = header.get(0).cloned().unwrap_or_default();
							let mut y_cols: Vec<String> = Vec::new();
							// treat any additional columns as metrics
							for c in header.iter().skip(1) { y_cols.push(c.clone()); }
							if !y_cols.is_empty() {
								chart = Some(api::FinalResponseResultChart::new(api::final_response_result_chart::Type::Line, x_col, y_cols));
							}
						}
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
						let mut res = api::FinalResponseResult { sql: result.sql, answer: final_answer, data: None, chart: None };
						// Attach data+chart if ask agent
						if agent == "ask" && !header.is_empty() && !rows.is_empty() {
							res.data = Some(api::FinalResponseResultData { header: header.clone(), rows: rows.clone() });
							if let Some(c) = chart { res.chart = Some(c); }
						}
						let resp = api::FinalResponse::new(1, m::final_response::Type::Final, now_iso(), state.next_seq(), thread_id.to_string(), tseq, res);
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						tracing::info!("WS -> {}", s);
						let _ = write.send(Message::Text(s)).await;
						// Append final_response step with the exact payload sent
						{
							let store = crate::qa::session::ThreadStore::new();
							let payload = &resp.result;
							let _ = store.append_step(thread_id, crate::qa::session::ThreadStep {
								action: "final_response".to_string(),
								args: serde_json::json!({
									"answer": payload.answer,
									"sql": payload.sql,
									"data": payload.data,
									"chart": payload.chart
								}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.to_string()),
							}).await;
						}
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
					Ok(RunOutcome::AwaitApproval { thread_id: _tid, prompt }) => {
						let tseq = state.next_thread_seq(thread_id);
						let outv = serde_json::json!({
							"v": 1,
							"type": "await_approval",
							"server_time": now_iso(),
							"seq": state.next_seq(),
							"thread_id": thread_id.to_string(),
							"thread_seq": tseq,
							"prompt": prompt,
						});
						let s = outv.to_string();
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

fn compact_schema_columns(fields: &[datafusion::arrow::datatypes::FieldRef]) -> Vec<(String, String)> {
	use datafusion::arrow::datatypes::{Field, DataType};
	fn walk(prefix: &str, f: &Field, depth: usize, out: &mut Vec<(String,String)>) {
		let name = if prefix.is_empty() { f.name().to_string() } else { format!("{}.{}", prefix, f.name()) };
		match f.data_type() {
			DataType::Struct(inner) if depth < 2 => {
                for child in inner.iter() {
                    walk(&name, child.as_ref(), depth + 1, out);
				}
			}
			dt => {
				out.push((name, format!("{:?}", dt)));
			}
		}
	}
	let mut out: Vec<(String,String)> = Vec::new();
	for f in fields {
		walk("", f.as_ref(), 0, &mut out);
	}
	out
}
enum AgentFrame {
	Final { answer: String, sql: Option<String> },
	AwaitUser { prompt: String },
	AwaitApproval { prompt: String },
}

async fn run_agent_and_frames(thread_id: &str, question: &str, agent: &str) -> Result<Vec<AgentFrame>, String> {
	// Delegate to flows
	let convert = |ff: crate::flows::adapter::FlowFrame| -> AgentFrame {
		match ff {
			crate::flows::adapter::FlowFrame::Final { answer, sql } => AgentFrame::Final { answer, sql },
			crate::flows::adapter::FlowFrame::AwaitUser { prompt } => AgentFrame::AwaitUser { prompt },
			crate::flows::adapter::FlowFrame::AwaitApproval { prompt } => AgentFrame::AwaitApproval { prompt },
			crate::flows::adapter::FlowFrame::Processing { .. } => AgentFrame::AwaitUser { prompt: "Processing...".to_string() },
		}
	};
	let frames = match agent {
		"model" => crate::flows::model::run(thread_id, question).await?.into_iter().map(convert).collect(),
		"cleanse" => crate::flows::cleanse::run(thread_id, question).await?.into_iter().map(convert).collect(),
		_ => crate::flows::ask::run(thread_id, question).await?.into_iter().map(convert).collect(),
	};
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
			"final" | "ask_user" | "ask_approval" => {
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
				"ask_approval" => {
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

