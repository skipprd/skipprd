use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
// Agent loop is invoked through suites; WS server doesn't call Agent directly.
// Removed unused tool imports; flows handle registry/tool selection
use uuid::Uuid;
use crate::ws::api_gen as api;
use std::collections::{HashMap, VecDeque};
use chrono::Utc;
use crate::models as m;
use react_core::session::ThreadStore;
use react_core::session::ThreadLog;
use react_core::session::ThreadStep;
use react_suites::registry::SuiteRegistry;
use react_suites::SuiteCtx;
use std::sync::Arc;

// Steering prompts removed for model agent; model runs eagerly without awaiting user choice.

/// Start the WebSocket server with an injected suite registry and context.
///
/// This is the preferred entrypoint for keeping `react` runtime generic: callers
/// decide how to build configuration, storage roots, credentials, etc.
pub async fn start_with_ctx(port: u16, suite_ctx: SuiteCtx) -> Result<(), String> {
    let reg = Arc::new(react_suites::default_registry());
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await.map_err(|e| e.to_string())?;
    tracing::info!("WebSocket server listening on ws://{}", addr);
    loop {
        let (stream, _sockaddr) = listener.accept().await.map_err(|e| e.to_string())?;
        let reg = reg.clone();
        let suite_ctx = suite_ctx.clone();
        tokio::spawn(async move {
            if let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await {
                let (mut write, mut read) = ws_stream.split();
				let mut state = ConnState::new(reg, suite_ctx);
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
			let store = state.thread_store();
			let ids = store.list().await;
			let mut threads: Vec<api::ListResponseThreadsInner> = Vec::new();
			for tid in ids {
				let mut item = api::ListResponseThreadsInner::new(tid.clone());
				if let Some(log) = store.get(&tid).await {
					let (suite_id, agent_type) = derive_thread_context(&log);
					// last_activity
					let last_ts = log.steps.last().map(|s| s.ts.clone());
					item.last_activity = last_ts;
					item.title = log.title.clone();
					item.suite_id = Some(suite_id);
					item.agent_type = Some(agent_type);
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
		"suites" => {
			let _req: Value = v.clone(); // tolerate schema drift; suites request is trivial
			let suites = build_suites_catalog(&state.reg);
			let outv = serde_json::json!({
				"v": 1,
				"type": "suites",
				"server_time": now_iso(),
				"seq": state.next_seq(),
				"defaultSuiteId": "data_engineer",
				"suites": suites,
			});
			let s = outv.to_string();
			state.buffer_last(&s);
			out.push(s);
		}
		"new" => {
			let req: api::NewRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let cid = req.cid.clone();
			let question = req.question.clone();
			if question.trim().is_empty() { return Err("question required".into()); }
			let thread_id = Uuid::new_v4().to_string();
			let suite_id = req.suite_id.clone();
			let agent = normalize_agent_new(req.agent_type);
			state.current_suite.insert(thread_id.clone(), suite_id.clone());
			state.current_agent.insert(thread_id.clone(), agent.clone());
			// Persist initial suite/agent selection so it survives reconnects
			{
				let store = state.thread_store();
				let _ = store.append_step(&thread_id, ThreadStep {
					action: "switch_suite".to_string(),
					args: serde_json::json!({"from": null, "to": suite_id.clone()}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(agent.clone()),
				}).await;
				let _ = store.append_step(&thread_id, ThreadStep {
					action: "switch_agent".to_string(),
					args: serde_json::json!({"from": null, "to": agent.clone()}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(agent.clone()),
				}).await;
			}
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
				let store = state.thread_store();
				let _ = store.append_step(&thread_id, ThreadStep {
					action: "user".to_string(),
					args: serde_json::json!({"text": question}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(agent.clone()),
				}).await;
				let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
			}
			// run agent
			let frames = run_agent_and_frames(&thread_id, &question, &suite_id, &agent, &state.reg, &state.suite_ctx).await?;
			for f in frames {
				match f {
					AgentFrame::Review { text, meta } => {
						let tseq = state.next_thread_seq(&thread_id);
						let mut rr = api::ReviewResponse::new(
							1,
							m::review_response::Type::Review,
							now_iso(),
							state.next_seq(),
							thread_id.clone(),
							tseq,
							text.clone(),
						);
						if let Some(v) = meta {
							if let Some(obj) = v.as_object() {
								let mut hm: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
								for (k, vv) in obj.iter() {
									hm.insert(k.clone(), vv.clone());
								}
								if !hm.is_empty() {
									rr.meta = Some(hm);
								}
							}
						}
						let resp = api::ServerMessage::Review(rr.clone());
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						{
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
								action: "review_response".to_string(),
								args: serde_json::json!({"text": text, "meta": rr.meta}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
					}
					AgentFrame::Final { answer, sql } => {
						let sql = if agent == "model" || agent == "cleanse" { None } else { sql };
						// finalize title once using concise summary
						{
							let store = state.thread_store();
							let title = synthesize_title(&state.suite_ctx.llm, &question, &answer).await;
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
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
								action: "final_response".to_string(),
								args: serde_json::json!({"answer": answer, "sql": sql, "data": null, "chart": null}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
						// Log entire thread on final
						{
							let store = state.thread_store();
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
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
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
						let resp = api::ServerMessage::AwaitApproval(api::AwaitApprovalResponse::new(
							1,
							m::await_approval_response::Type::AwaitApproval,
							now_iso(),
							state.next_seq(),
							thread_id.clone(),
							tseq,
							prompt,
						));
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
			let requested_suite = req.suite_id.clone();
			let requested_agent = normalize_agent_open(req.agent_type);

			// Derive current suite/agent from persisted thread log (durable across reconnects)
			let store = state.thread_store();
			let (current_suite, current_agent) = match store.get(&thread_id).await {
				Some(log) => derive_thread_context(&log),
				None => ("data_engineer".to_string(), "ask".to_string()),
			};
			// Track suite per-thread (explicit client selection) and persist switch if changed
			if current_suite != requested_suite {
				let _ = store.append_step(&thread_id, ThreadStep {
					action: "switch_suite".to_string(),
					args: serde_json::json!({"from": current_suite.clone(), "to": requested_suite.clone()}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(requested_agent.clone()),
				}).await;
			}
			state.current_suite.insert(thread_id.clone(), requested_suite.clone());

			if current_agent != requested_agent {
				// append switch_agent step
				let _ = store.append_step(&thread_id, ThreadStep {
					action: "switch_agent".to_string(),
					args: serde_json::json!({"from": current_agent.clone(), "to": requested_agent.clone()}),
					observation: serde_json::json!({"ok": true}),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: Some(requested_agent.clone()),
				}).await;
				state.current_agent.insert(thread_id.clone(), requested_agent.clone());
			}
			// Ensure we always persist the explicitly requested agent for this thread
			state.current_agent.insert(thread_id.clone(), requested_agent.clone());
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
			let suite_id = state.current_suite.get(&thread_id).cloned().unwrap_or_else(|| requested_suite.clone());
			let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| requested_agent.clone());
			let frames = run_agent_and_frames(&thread_id, &question, &suite_id, &agent, &state.reg, &state.suite_ctx).await?;
			for f in frames {
				match f {
					AgentFrame::Review { text, meta } => {
						let tseq = state.next_thread_seq(&thread_id);
						let mut rr = api::ReviewResponse::new(
							1,
							m::review_response::Type::Review,
							now_iso(),
							state.next_seq(),
							thread_id.clone(),
							tseq,
							text.clone(),
						);
						if let Some(v) = meta {
							if let Some(obj) = v.as_object() {
								let mut hm: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
								for (k, vv) in obj.iter() {
									hm.insert(k.clone(), vv.clone());
								}
								if !hm.is_empty() {
									rr.meta = Some(hm);
								}
							}
						}
						let resp = api::ServerMessage::Review(rr.clone());
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						{
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
								action: "review_response".to_string(),
								args: serde_json::json!({"text": text, "meta": rr.meta}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
					}
					AgentFrame::Final { answer, sql } => {
						let sql = if agent == "model" || agent == "cleanse" { None } else { sql };
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
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
								action: "final_response".to_string(),
								args: serde_json::json!({"answer": answer, "sql": sql, "data": null, "chart": null}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(agent.clone()),
							}).await;
						}
						// Log entire thread on final
						{
							let store = state.thread_store();
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
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
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
						let resp = api::ServerMessage::AwaitApproval(api::AwaitApprovalResponse::new(
							1,
							m::await_approval_response::Type::AwaitApproval,
							now_iso(),
							state.next_seq(),
							thread_id.clone(),
							tseq,
							prompt,
						));
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
			// Ensure durable suite/agent state is available after reconnect
			if !state.current_suite.contains_key(&thread_id) || !state.current_agent.contains_key(&thread_id) {
				let store = state.thread_store();
				if let Some(log) = store.get(&thread_id).await {
					let (suite_id, agent_type) = derive_thread_context(&log);
					state.current_suite.insert(thread_id.clone(), suite_id);
					state.current_agent.insert(thread_id.clone(), agent_type);
				}
			}
			let store = state.thread_store();
			let _ = store.append_step(&thread_id, ThreadStep {
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
			let suite_id = state
				.current_suite
				.get(&thread_id)
				.cloned()
				.ok_or_else(|| "suite_id missing for thread".to_string())?;
			let agent = state
				.current_agent
				.get(&thread_id)
				.cloned()
				.ok_or_else(|| "agent_type missing for thread".to_string())?;
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
			let store2 = state.thread_store();
			let mut q_for_resume = "Continue.".to_string();
			if let Some(log) = store2.get(&thread_id).await {
				if let Some(first_user) = log.steps.iter().find(|s| s.action == "user") {
					if let Some(t) = first_user.args.get("text").and_then(|x| x.as_str()) {
						if !t.trim().is_empty() { q_for_resume = t.to_string(); }
					}
				}
			}
			let frames = run_agent_and_frames(&thread_id, &q_for_resume, &suite_id, &agent, &state.reg, &state.suite_ctx).await?;
			for f in frames {
				match f {
					AgentFrame::Review { text, meta } => {
						let tseq = state.next_thread_seq(&thread_id);
						let mut rr = api::ReviewResponse::new(
							1,
							m::review_response::Type::Review,
							now_iso(),
							state.next_seq(),
							thread_id.clone(),
							tseq,
							text.clone(),
						);
						if let Some(v) = meta {
							if let Some(obj) = v.as_object() {
								let mut hm: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
								for (k, vv) in obj.iter() {
									hm.insert(k.clone(), vv.clone());
								}
								if !hm.is_empty() {
									rr.meta = Some(hm);
								}
							}
						}
						let resp = api::ServerMessage::Review(rr.clone());
						let s = serde_json::to_string(&resp).unwrap();
						state.buffer_last(&s);
						out.push(s);
						{
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
								action: "review_response".to_string(),
								args: serde_json::json!({"text": text, "meta": rr.meta}),
								observation: serde_json::json!({"ok": true}),
								ts: chrono::Utc::now().to_rfc3339(),
								agent: Some(state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| "ask".to_string())),
							}).await;
						}
					}
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
							let store = state.thread_store();
							let _ = store.append_step(&thread_id, ThreadStep {
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
						let resp = api::ServerMessage::AwaitApproval(api::AwaitApprovalResponse::new(
							1,
							m::await_approval_response::Type::AwaitApproval,
							now_iso(),
							state.next_seq(),
							thread_id.clone(),
							tseq,
							prompt,
						));
						let s = serde_json::to_string(&resp).unwrap();
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
			let store = state.thread_store();
			let (messages, next_before) = build_history(&store, &thread_id, req.before_thread_seq, req.limit).await;
			let mut resp = api::HistoryResponse::new(1, m::history_response::Type::History, now_iso(), state.next_seq(), thread_id.clone(), messages);
			if let Some(log) = store.get(&thread_id).await {
				let (suite_id, agent_type) = derive_thread_context(&log);
				resp.title = log.title;
				resp.suite_id = Some(suite_id);
				resp.agent_type = Some(agent_type);
			}
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
			let store = state.thread_store();
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
				let store = state.thread_store();
				let _ = store.delete(&thread_id).await;
			}
			// best-effort vector cleanup (optional provider)
			if let Some(vs) = state.suite_ctx.vector.as_ref() {
                let _ = vs.delete_thread_embeddings(&state.suite_ctx.scope, &thread_id).await;
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

fn env_truthy(key: &str) -> bool {
	std::env::var(key)
		.ok()
		.map(|v| {
			let vv = v.trim().to_lowercase();
			vv == "1" || vv == "true" || vv == "yes"
		})
		.unwrap_or(false)
}

fn truncate_str(s: &str, max: usize) -> String {
	if s.len() <= max { return s.to_string(); }
	match s.char_indices().take_while(|(i, _)| *i < max).last() {
		Some((i, _)) => format!("{}…", &s[..i]),
		None => s.chars().take(max).collect(),
	}
}

fn summarize_step(step: &ThreadStep) -> String {
	let args = truncate_str(&step.args.to_string(), 160);
	let obs = truncate_str(&step.observation.to_string(), 160);
	let agent = step.agent.clone().unwrap_or_else(|| "unknown".to_string());
	format!("ts={} agent={} action={} args={} obs={}", step.ts, agent, step.action, args, obs)
}

async fn log_thread_steps_if_enabled(store: &ThreadStore, thread_id: &str, reason: &str) {
	if !env_truthy("REACT_LOG_THREAD_STEPS") {
		return;
	}
	match store.get(thread_id).await {
		Some(log) => {
			tracing::info!(
				"THREAD_LOG {} thread_id={} steps={} title={:?} finalized={}",
				reason,
				thread_id,
				log.steps.len(),
				log.title,
				log.title_finalized
			);
			// Print last N steps (most useful when debugging long loops).
			let n = 40usize;
			let start = log.steps.len().saturating_sub(n);
			for (i, step) in log.steps.iter().enumerate().skip(start) {
				tracing::info!("THREAD_STEP {} #{} {}", thread_id, i + 1, summarize_step(step));
			}
			// Optional full JSON dump (very verbose).
			if env_truthy("REACT_LOG_THREAD_JSON") {
				if let Ok(pretty) = serde_json::to_string_pretty(&log) {
					tracing::info!("THREAD_JSON {} {}", thread_id, pretty);
				}
			}
		}
		None => tracing::info!("THREAD_LOG {} thread_id={} (not found)", reason, thread_id),
	}
}

fn truncate_title(s: &str, max_chars: usize) -> String {
	if s.len() <= max_chars { return s.to_string(); }
	match s.char_indices().take_while(|(i, _)| *i < max_chars).last() {
		Some((i, _)) => format!("{}…", &s[..i]),
		None => s.chars().take(max_chars).collect(),
	}
}

async fn synthesize_title(llm: &react_core::llm::DynLlm, question: &str, answer: &str) -> String {
	// Try LLM to produce a concise title (<= 8 words), else fallback to truncated question
	let prompt = format!(
		"Create a very short, descriptive chat title (≤ 8 words).\nRules: plain text only, no quotes, no punctuation beyond spaces, title case.\nQuestion: {}\nAnswer: {}\nTitle:",
		question, answer
	);
	let out = tokio::task::spawn_blocking({
		let llm2 = llm.clone();
		let p = prompt.clone();
		move || llm2.chat(&[react_core::llm::ChatMessage { role: "user".into(), content: p }])
	}).await;
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
	current_suite: HashMap<String, String>,
	current_agent: HashMap<String, String>,
	reg: Arc<SuiteRegistry>,
	suite_ctx: SuiteCtx,
}

async fn load_last_run_sql_async(thread_id: &str) -> (Vec<String>, Vec<Vec<String>>) {
	// NOTE: This helper is currently unused. Keep it as a stub to avoid coupling
	// to the thread store from this generic WS module.
	let _ = thread_id;
	(Vec::new(), Vec::new())
}

async fn synthesize_summary(question: &str, agent_answer: &str, sql_opt: &Option<String>, header: &[String], rows: &[Vec<String>]) -> Option<String> {
	// NOTE: Currently unused. Kept as a stub to avoid coupling WS core to LLM bootstrap/config.
	let _ = (question, agent_answer, sql_opt, header, rows);
	None
}

impl ConnState {
	fn new(reg: Arc<SuiteRegistry>, suite_ctx: SuiteCtx) -> Self {
		Self {
			seq: 0,
			sent: VecDeque::new(),
			thread_seq: HashMap::new(),
			seen: HashMap::new(),
			current_suite: HashMap::new(),
			current_agent: HashMap::new(),
			reg,
			suite_ctx,
		}
	}
	fn thread_store(&self) -> ThreadStore {
		ThreadStore::new(
			self.suite_ctx.storage.clone(),
			self.suite_ctx.scope.clone(),
			self.suite_ctx.keyspace.clone(),
		)
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

fn derive_thread_context(log: &ThreadLog) -> (String, String) {
	// Defaults for back-compat threads with no recorded context.
	let mut suite_id = "data_engineer".to_string();
	let mut agent_type = "ask".to_string();
	for step in log.steps.iter() {
		if step.action == "switch_suite" {
			if let Some(to) = step.args.get("to").and_then(|x| x.as_str()) {
				if !to.trim().is_empty() {
					suite_id = to.to_string();
				}
			}
		}
		if step.action == "switch_agent" {
			if let Some(to) = step.args.get("to").and_then(|x| x.as_str()) {
				if !to.trim().is_empty() {
					agent_type = to.to_string();
				}
			}
		}
		// Best-effort: if the step recorded an agent, treat it as the latest known mode.
		if let Some(a) = step.agent.as_ref() {
			if !a.trim().is_empty() {
				agent_type = a.to_string();
			}
		}
	}
	(suite_id, agent_type)
}

fn build_suites_catalog(reg: &SuiteRegistry) -> Vec<serde_json::Value> {
	let mut out: Vec<serde_json::Value> = Vec::new();
	for id in reg.list_ids() {
		match id {
			"data_engineer" => out.push(serde_json::json!({
				"suiteId": "data_engineer",
				"label": "Data Engineer",
				"allowedAgentTypes": ["ask", "model", "cleanse", "review", "agent"],
				"defaultAgentType": "ask",
			})),
			"kb" => out.push(serde_json::json!({
				"suiteId": "kb",
				"label": "KB",
				"allowedAgentTypes": ["kb"],
				"defaultAgentType": "kb",
			})),
			_ => out.push(serde_json::json!({
				"suiteId": id,
				"label": serde_json::Value::Null,
				"allowedAgentTypes": ["ask"],
				"defaultAgentType": "ask",
			})),
		}
	}
	out
}

async fn process_new(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let req: api::NewRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
	let cid = req.cid.clone();
	let question = req.question.clone();
	if question.trim().is_empty() { return Err("question required".into()); }
	let thread_id = Uuid::new_v4().to_string();
	let suite_id = req.suite_id.clone();
	let agent = normalize_agent_new(req.agent_type);
	state.current_suite.insert(thread_id.clone(), suite_id.clone());
	state.current_agent.insert(thread_id.clone(), agent.clone());
	// Persist initial suite/agent selection so it survives reconnects
	{
		let store = state.thread_store();
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "switch_suite".to_string(),
			args: serde_json::json!({"from": null, "to": suite_id.clone()}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "switch_agent".to_string(),
			args: serde_json::json!({"from": null, "to": agent.clone()}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
	}
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
		let store = state.thread_store();
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": question}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
		let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
	}
	run_agent_with_processing(&thread_id, &question, &suite_id, &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}

async fn process_open(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let req: api::OpenRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
	let cid = req.cid.clone();
	let thread_id = req.thread_id.clone();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	let question = req.question.clone().unwrap_or_else(|| "Continue.".to_string());
	let requested_suite = req.suite_id.clone();
	let requested_agent = normalize_agent_open(req.agent_type);

	// Derive current suite/agent from persisted thread log (durable across reconnects)
	let store = state.thread_store();
	let (current_suite, current_agent) = match store.get(&thread_id).await {
		Some(log) => derive_thread_context(&log),
		None => ("data_engineer".to_string(), "ask".to_string()),
	};
	if current_suite != requested_suite {
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "switch_suite".to_string(),
			args: serde_json::json!({"from": current_suite.clone(), "to": requested_suite.clone()}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(requested_agent.clone()),
		}).await;
	}
	state.current_suite.insert(thread_id.clone(), requested_suite.clone());

	if current_agent != requested_agent {
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "switch_agent".to_string(),
			args: serde_json::json!({"from": current_agent.clone(), "to": requested_agent.clone()}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(requested_agent.clone()),
		}).await;
		state.current_agent.insert(thread_id.clone(), requested_agent.clone());
	}
	// Ensure we always persist the explicitly requested agent for this thread
	state.current_agent.insert(thread_id.clone(), requested_agent.clone());
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
	let suite_id = state.current_suite.get(&thread_id).cloned().unwrap_or_else(|| requested_suite.clone());
	let agent = state.current_agent.get(&thread_id).cloned().unwrap_or_else(|| requested_agent.clone());
	// if user supplied a prompt on open, record it
	if !question.trim().is_empty() {
		let store = state.thread_store();
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": question}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
		let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
	}
	run_agent_with_processing(&thread_id, &question, &suite_id, &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}

async fn process_approve(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let cid = v.get("cid").and_then(|x| x.as_str()).ok_or_else(|| "cid required".to_string())?.to_string();
	let thread_id = v.get("thread_id").and_then(|x| x.as_str()).ok_or_else(|| "thread_id required".to_string())?.to_string();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	// Reconnect-safe: derive suite/agent from persisted history if not present in connection state
	if !state.current_suite.contains_key(&thread_id) || !state.current_agent.contains_key(&thread_id) {
		let store = state.thread_store();
		if let Some(log) = store.get(&thread_id).await {
			let (suite_id, agent_type) = derive_thread_context(&log);
			state.current_suite.insert(thread_id.clone(), suite_id);
			state.current_agent.insert(thread_id.clone(), agent_type);
		}
	}
	let suite_id = state
		.current_suite
		.get(&thread_id)
		.cloned()
		.ok_or_else(|| "suite_id missing for thread".to_string())?;
	let agent = state
		.current_agent
		.get(&thread_id)
		.cloned()
		.ok_or_else(|| "agent_type missing for thread".to_string())?;
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
		let store = state.thread_store();
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": "approve"}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
	}
	tracing::info!("approve: thread_id={} agent={}", thread_id, agent);
	run_agent_with_processing(&thread_id, "Continue.", &suite_id, &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}

async fn process_reject(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let cid = v.get("cid").and_then(|x| x.as_str()).ok_or_else(|| "cid required".to_string())?.to_string();
	let thread_id = v.get("thread_id").and_then(|x| x.as_str()).ok_or_else(|| "thread_id required".to_string())?.to_string();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	// Reconnect-safe: derive suite/agent from persisted history if not present in connection state
	if !state.current_suite.contains_key(&thread_id) || !state.current_agent.contains_key(&thread_id) {
		let store = state.thread_store();
		if let Some(log) = store.get(&thread_id).await {
			let (suite_id, agent_type) = derive_thread_context(&log);
			state.current_suite.insert(thread_id.clone(), suite_id);
			state.current_agent.insert(thread_id.clone(), agent_type);
		}
	}
	let suite_id = state
		.current_suite
		.get(&thread_id)
		.cloned()
		.ok_or_else(|| "suite_id missing for thread".to_string())?;
	let agent = state
		.current_agent
		.get(&thread_id)
		.cloned()
		.ok_or_else(|| "agent_type missing for thread".to_string())?;
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
		let store = state.thread_store();
		let _ = store.append_step(&thread_id, ThreadStep {
			action: "user".to_string(),
			args: serde_json::json!({"text": "reject"}),
			observation: serde_json::json!({"ok": true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some(agent.clone()),
		}).await;
	}
	tracing::info!("reject: thread_id={} agent={}", thread_id, agent);
	run_agent_with_processing(&thread_id, "Continue.", &suite_id, &agent, &cid, state, write, m::processing_response::Stage::Queued).await
}
// normalize_agent removed (unused)

fn normalize_agent_new(a: api::new_request::AgentType) -> String {
	match a {
		api::new_request::AgentType::Cleanse => "cleanse".to_string(),
		api::new_request::AgentType::Model => "model".to_string(),
		api::new_request::AgentType::Ask => "ask".to_string(),
		api::new_request::AgentType::Kb => "kb".to_string(),
		api::new_request::AgentType::Agent => "agent".to_string(),
		api::new_request::AgentType::Review => "review".to_string(),
	}
}

fn normalize_agent_open(a: api::open_request::AgentType) -> String {
	match a {
		api::open_request::AgentType::Cleanse => "cleanse".to_string(),
		api::open_request::AgentType::Model => "model".to_string(),
		api::open_request::AgentType::Ask => "ask".to_string(),
		api::open_request::AgentType::Kb => "kb".to_string(),
		api::open_request::AgentType::Agent => "agent".to_string(),
		api::open_request::AgentType::Review => "review".to_string(),
	}
}
async fn run_agent_with_processing(
	thread_id: &str,
	question: &str,
	suite_id: &str,
	agent: &str,
	cid: &str,
	state: &mut ConnState,
	write: &mut (impl SinkExt<Message> + Unpin),
	initial_stage: m::processing_response::Stage,
) -> Result<(), String> {
	// Suite-based runner. We intentionally keep this simple: the suite owns prompts/tools and the core
	// WS server only handles message I/O. Step-level progress streaming can be reintroduced later by
	// threading progress channels through the suite runner.
	return run_agent_with_processing_suite(thread_id, question, suite_id, agent, cid, state, write, initial_stage).await;
	}
fn compact_schema_columns(fields: &[arrow::datatypes::FieldRef]) -> Vec<(String, String)> {
	use arrow::datatypes::{Field, DataType};
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
	Review { text: String, meta: Option<serde_json::Value> },
	AwaitUser { prompt: String },
	AwaitApproval { prompt: String },
}

async fn run_agent_with_processing_suite(
	thread_id: &str,
	question: &str,
	suite_id: &str,
	agent: &str,
	cid: &str,
	state: &mut ConnState,
	write: &mut (impl SinkExt<Message> + Unpin),
	_initial_stage: m::processing_response::Stage,
) -> Result<(), String> {
	// Optional trace streaming: allow the agent loop to emit short debug lines and forward them
	// to the client as `processing` frames (extra field `message`).
	let trace_enabled = std::env::var("REACT_TRACE")
		.ok()
		.map(|v| {
			let vv = v.trim().to_lowercase();
			vv == "1" || vv == "true" || vv == "yes"
		})
		.unwrap_or(false);

	let (trace_tx, mut trace_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
	let mut sctx2 = state.suite_ctx.clone();
	if trace_enabled {
		sctx2.trace_tx = Some(trace_tx);
	}

	let suite = state
		.reg
		.get(suite_id)
		.ok_or_else(|| format!("invalid suite_id '{}'", suite_id))?
		.clone();
	let thread_id_s = thread_id.to_string();
	let q_s = question.to_string();
	let agent_s = agent.to_string();

	let agent_task = tokio::spawn(async move { suite.handle_open(&thread_id_s, &q_s, &agent_s, &sctx2).await });
	tokio::pin!(agent_task);

	// While the suite/agent is running, forward trace lines (if enabled).
	loop {
		tokio::select! {
			Some(line) = trace_rx.recv() => {
				if !trace_enabled { continue; }
				let msg = if line.len() > 260 { format!("{}…", &line[..260]) } else { line };
				let outv = serde_json::json!({
					"v": 1,
					"type": "processing",
					"server_time": now_iso(),
					"seq": state.next_seq(),
					"thread_id": thread_id,
					"for_cid": cid,
					"stage": "processing",
					"message": msg,
				});
				let s = outv.to_string();
				state.buffer_last(&s);
				tracing::info!("WS -> {}", s);
				let _ = write.send(Message::Text(s)).await;
			}
			res = &mut agent_task => {
				let frames = match res {
					Ok(Ok(v)) => v,
					Ok(Err(e)) => return Err(e),
					Err(e) => return Err(format!("agent task failed: {}", e)),
				};
				let convert = |ff: react_suites::FlowFrame| -> AgentFrame {
					match ff {
						react_suites::FlowFrame::Final { answer, sql } => AgentFrame::Final { answer, sql },
						react_suites::FlowFrame::Review { text, meta } => AgentFrame::Review { text, meta },
						react_suites::FlowFrame::AwaitUser { prompt } => AgentFrame::AwaitUser { prompt },
						react_suites::FlowFrame::AwaitApproval { prompt } => AgentFrame::AwaitApproval { prompt },
					}
				};
				let frames = frames.into_iter().map(convert).collect::<Vec<_>>();
				// Continue with normal WS frame emission below.
				let frames = frames;

				for f in frames {
					match f {
						AgentFrame::Review { text, meta } => {
							let tseq = state.next_thread_seq(thread_id);
							let mut resp = api::ReviewResponse::new(
								1,
								m::review_response::Type::Review,
								now_iso(),
								state.next_seq(),
								thread_id.to_string(),
								tseq,
								text.clone(),
							);
							// Convert meta value to ReviewResponse's map type (best-effort).
							if let Some(v) = meta {
								if let Some(obj) = v.as_object() {
									let mut hm: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
									for (k, vv) in obj.iter() {
										hm.insert(k.clone(), vv.clone());
									}
									if !hm.is_empty() {
										resp.meta = Some(hm);
									}
								}
							}
							let s = serde_json::to_string(&resp).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;

							// Persist as its own step so history can show reviewer output.
							{
								let store = state.thread_store();
								let _ = store
									.append_step(
										thread_id,
										ThreadStep {
											action: "review_response".to_string(),
											args: serde_json::json!({"text": text, "meta": resp.meta}),
											observation: serde_json::json!({"ok": true}),
											ts: chrono::Utc::now().to_rfc3339(),
											agent: Some(agent.to_string()),
										},
									)
									.await;
							}
							// Review is non-terminal; continue emitting subsequent frames.
							continue;
						}
						AgentFrame::Final { answer, sql } => {
							let sql = if agent == "model" || agent == "cleanse" { None } else { sql };
							for t in chunk_text(&answer, 24) {
								let mut tk = api::TokenResponse::new(1, m::token_response::Type::Token, now_iso(), state.next_seq(), thread_id.to_string(), t);
								tk.for_cid = Some(cid.to_string());
								let s = serde_json::to_string(&tk).unwrap();
								state.buffer_last(&s);
								tracing::info!("WS -> {}", s);
								let _ = write.send(Message::Text(s)).await;
							}
							let tseq = state.next_thread_seq(thread_id);
							let resp = api::FinalResponse::new(
								1,
								m::final_response::Type::Final,
								now_iso(),
								state.next_seq(),
								thread_id.to_string(),
								tseq,
								api::FinalResponseResult { sql: sql.clone(), answer: answer.clone(), data: None, chart: None },
							);
							let s = serde_json::to_string(&resp).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;

							// Append final_response step with the exact payload sent
							{
								let store = state.thread_store();
								let _ = store
									.append_step(
										thread_id,
										ThreadStep {
											action: "final_response".to_string(),
											args: serde_json::json!({"answer": answer, "sql": sql, "data": null, "chart": null}),
											observation: serde_json::json!({"ok": true}),
											ts: chrono::Utc::now().to_rfc3339(),
											agent: Some(agent.to_string()),
										},
									)
									.await;
							}
							// Debug: print persisted thread steps
							{
								let store = state.thread_store();
								log_thread_steps_if_enabled(&store, thread_id, "final").await;
							}
							return Ok(());
						}
						AgentFrame::AwaitUser { prompt } => {
							let tseq = state.next_thread_seq(thread_id);
							let resp = api::AwaitUserResponse::new(
								1,
								m::await_user_response::Type::AwaitUser,
								now_iso(),
								state.next_seq(),
								thread_id.to_string(),
								tseq,
								prompt,
							);
							let s = serde_json::to_string(&resp).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;
							{
								let store = state.thread_store();
								log_thread_steps_if_enabled(&store, thread_id, "await_user").await;
							}
							return Ok(());
						}
						AgentFrame::AwaitApproval { prompt } => {
							let tseq = state.next_thread_seq(thread_id);
							let resp = api::AwaitApprovalResponse::new(
								1,
								m::await_approval_response::Type::AwaitApproval,
								now_iso(),
								state.next_seq(),
								thread_id.to_string(),
								tseq,
								prompt,
							);
							let s = serde_json::to_string(&resp).unwrap();
							state.buffer_last(&s);
							tracing::info!("WS -> {}", s);
							let _ = write.send(Message::Text(s)).await;
							{
								let store = state.thread_store();
								log_thread_steps_if_enabled(&store, thread_id, "await_approval").await;
							}
							return Ok(());
						}
					}
				}
				return Ok(());
			}
		}
	}
}

async fn run_agent_and_frames(
	thread_id: &str,
	question: &str,
	suite_id: &str,
	agent: &str,
	reg: &SuiteRegistry,
	sctx: &SuiteCtx,
) -> Result<Vec<AgentFrame>, String> {
	// Delegate to suites
    let convert = |ff: react_suites::FlowFrame| -> AgentFrame {
		match ff {
			react_suites::FlowFrame::Final { answer, sql } => AgentFrame::Final { answer, sql },
			react_suites::FlowFrame::Review { text, meta } => AgentFrame::Review { text, meta },
			react_suites::FlowFrame::AwaitUser { prompt } => AgentFrame::AwaitUser { prompt },
			react_suites::FlowFrame::AwaitApproval { prompt } => AgentFrame::AwaitApproval { prompt },
		}
	};
	let suite = reg
		.get(suite_id)
		.ok_or_else(|| format!("invalid suite_id '{}'", suite_id))?;
	let frames = suite
		.handle_open(thread_id, question, agent, sctx)
		.await?
		.into_iter()
		.map(convert)
		.collect();
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

fn compute_unread_for_log(log: &ThreadLog, seen_seq: i32) -> (i32, i32) {
	let mut tseq: i32 = 0;
	let mut assistant_count_after_seen: i32 = 0;
	let mut last_assistant_seq: i32 = 0;
	for step in log.steps.iter() {
		match step.action.as_str() {
			"user" => {
				tseq += 1;
			}
			"final" | "ask_user" | "ask_approval" | "review_response" => {
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

async fn build_history(store: &ThreadStore, thread_id: &str, before: Option<i32>, limit_opt: Option<i32>) -> (Vec<api::HistoryResponseMessagesInner>, Option<i32>) {
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
				"review_response" => {
					tseq += 1;
					let content = step.args.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::adapters::storage::InMemoryStorageAdapter;
	use crate::providers::{DefaultKeyspace, RequestScope};
	use serde_json::json;
	use std::sync::Arc;

	#[test]
	fn normalize_agent_includes_agent_and_review() {
		assert_eq!(normalize_agent_new(api::new_request::AgentType::Agent), "agent");
		assert_eq!(normalize_agent_new(api::new_request::AgentType::Review), "review");
		assert_eq!(normalize_agent_open(api::open_request::AgentType::Agent), "agent");
		assert_eq!(normalize_agent_open(api::open_request::AgentType::Review), "review");
	}

	#[test]
	fn unread_counts_include_review_response() {
		let log = ThreadLog {
			steps: vec![
				ThreadStep {
					action: "user".to_string(),
					args: json!({"text":"hi"}),
					observation: json!({"ok":true}),
					ts: "t".to_string(),
					agent: Some("ask".to_string()),
				},
				ThreadStep {
					action: "review_response".to_string(),
					args: json!({"text":"review text","meta":null}),
					observation: json!({"ok":true}),
					ts: "t".to_string(),
					agent: Some("agent".to_string()),
				},
				ThreadStep {
					action: "final".to_string(),
					args: json!({"answer":"done","sql":null}),
					observation: json!({"ok":true}),
					ts: "t".to_string(),
					agent: Some("agent".to_string()),
				},
			],
			..Default::default()
		};
		// No seen messages yet -> both assistant messages should count as unread.
		let (_max_seq, unread) = compute_unread_for_log(&log, 0);
		assert_eq!(unread, 2);
	}

	#[tokio::test]
	async fn history_includes_review_response_as_assistant_message() {
		let storage = Arc::new(InMemoryStorageAdapter::default());
		let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
		let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
		let store = ThreadStore::new(storage, scope, keyspace);

		let tid = "thread1";
		let _ = store.append_step(tid, ThreadStep {
			action: "user".to_string(),
			args: json!({"text":"start"}),
			observation: json!({"ok":true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some("ask".to_string()),
		}).await;

		let _ = store.append_step(tid, ThreadStep {
			action: "review_response".to_string(),
			args: json!({"text":"review text","meta":null}),
			observation: json!({"ok":true}),
			ts: chrono::Utc::now().to_rfc3339(),
			agent: Some("agent".to_string()),
		}).await;

		let (msgs, _next) = build_history(&store, tid, None, Some(50)).await;
		assert!(msgs.iter().any(|m| m.role == m::history_response_messages_inner::Role::Assistant && m.content == "review text"));
	}
}

