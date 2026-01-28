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
use react_core::session::{Observation, ThreadStore, ThreadLog, ThreadStep, ToolObservation};
use react_suites::registry::SuiteRegistry;
use react_suites::SuiteCtx;
use react_suites::data_engineer::plan as de_plan;
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
									} else if t == "user" {
										if let Err(e) = process_user(&v, &mut state, &mut write).await {
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
				if let Ok(log) = store.get(&tid).await {
					let (suite_id, agent_type) = derive_thread_context(&log);
					// last_activity
					item.last_activity = log.steps.last().map(|s| s.ts().to_string());
					item.title = log.title.clone();
					item.suite_id = Some(suite_id);
					item.agent_type = Some(agent_type);
					// compute preview from last user or final
					let mut preview: Option<String> = None;
					for step in log.steps.iter().rev() {
						match step {
							ThreadStep::User { text, .. } => {
								preview = Some(text.to_string());
								break;
							}
							ThreadStep::Final { answer, .. } => {
								preview = Some(answer.to_string());
								break;
							}
							_ => {}
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
			let _req: api::SuitesRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let suites = build_suites_catalog(&state.reg);
			let mut resp = api::SuitesResponse::new(
				1,
				m::suites_response::Type::Suites,
				now_iso(),
				state.next_seq(),
				suites,
			);
			resp.default_suite_id = Some("data_engineer".to_string());
			let s = serde_json::to_string(&resp).unwrap();
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
			let agent = normalize_agent_new(req.agent_type)?;
			state.current_suite.insert(thread_id.clone(), suite_id.clone());
			state.current_agent.insert(thread_id.clone(), agent.clone());
			// Persist initial suite/agent selection so it survives reconnects
			{
				let store = state.thread_store();
				let _ = store
					.append_step(
						&thread_id,
						ThreadStep::SwitchSuite {
							from: None,
							to: suite_id.clone(),
							observation: Observation::ok(),
							ts: chrono::Utc::now().to_rfc3339(),
							agent: agent.clone(),
						},
					)
					.await;
				let _ = store
					.append_step(
						&thread_id,
						ThreadStep::SwitchAgent {
							from: None,
							to: agent.clone(),
							observation: Observation::ok(),
							ts: chrono::Utc::now().to_rfc3339(),
							agent: agent.clone(),
						},
					)
					.await;
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
				let _ = store
					.append_step(
						&thread_id,
						ThreadStep::User {
							text: question.clone(),
							observation: Observation::ok(),
							ts: chrono::Utc::now().to_rfc3339(),
							agent: agent.clone(),
						},
					)
					.await;
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
							let meta_val: Option<Value> = rr
								.meta
								.as_ref()
								.and_then(|m| serde_json::to_value(m).ok());
							let _ = store
								.append_step(
									&thread_id,
									ThreadStep::ReviewResponse {
										text: text.clone(),
										meta: meta_val,
										observation: Observation::ok(),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: agent.clone(),
									},
								)
								.await;
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
						// Log entire thread on final
						{
							let store = state.thread_store();
							if let Ok(log) = store.get(&thread_id).await {
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
							let _ = store
								.append_step(
									&thread_id,
									ThreadStep::AskUser {
										prompt,
										observation: Observation::ok(),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: agent.clone(),
									},
								)
								.await;
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
			let requested_agent = normalize_agent_open(req.agent_type)?;

			// Derive current suite/agent from persisted thread log (durable across reconnects)
			let store = state.thread_store();
			let log = store.get(&thread_id).await?;
			let (current_suite, current_agent) = derive_thread_context(&log);
			// Track suite per-thread (explicit client selection) and persist switch if changed
			if current_suite != requested_suite {
				let _ = store
					.append_step(
						&thread_id,
						ThreadStep::SwitchSuite {
							from: Some(current_suite.clone()),
							to: requested_suite.clone(),
							observation: Observation::ok(),
							ts: chrono::Utc::now().to_rfc3339(),
							agent: requested_agent.clone(),
						},
					)
					.await;
			}
			state.current_suite.insert(thread_id.clone(), requested_suite.clone());

			if current_agent != requested_agent {
				// append switch_agent step
				let _ = store
					.append_step(
						&thread_id,
						ThreadStep::SwitchAgent {
							from: Some(current_agent.clone()),
							to: requested_agent.clone(),
							observation: Observation::ok(),
							ts: chrono::Utc::now().to_rfc3339(),
							agent: requested_agent.clone(),
						},
					)
					.await;
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
							let meta_val: Option<Value> = rr
								.meta
								.as_ref()
								.and_then(|m| serde_json::to_value(m).ok());
							let _ = store
								.append_step(
									&thread_id,
									ThreadStep::ReviewResponse {
										text: text.clone(),
										meta: meta_val,
										observation: Observation::ok(),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: agent.clone(),
									},
								)
								.await;
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
						// Log entire thread on final
						{
							let store = state.thread_store();
							if let Ok(log) = store.get(&thread_id).await {
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
							let _ = store
								.append_step(
									&thread_id,
									ThreadStep::AskUser {
										prompt,
										observation: Observation::ok(),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: agent.clone(),
									},
								)
								.await;
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
				let log = store.get(&thread_id).await?;
				let (suite_id, agent_type) = derive_thread_context(&log);
				state.current_suite.insert(thread_id.clone(), suite_id);
				state.current_agent.insert(thread_id.clone(), agent_type);
			}
			let store = state.thread_store();
			let agent_label = state
				.current_agent
				.get(&thread_id)
				.cloned()
				.unwrap_or_else(|| "ask".to_string());
			let _ = store
				.append_step(
					&thread_id,
					ThreadStep::User {
						text: text.clone(),
						observation: Observation::ok(),
						ts: chrono::Utc::now().to_rfc3339(),
						agent: agent_label,
					},
				)
				.await;
			// ack
			// we don't increment thread_seq on user ack
			let mut ok = api::OkResponse::new(1, m::ok_response::Type::Ok, now_iso());
			ok.cid = Some(req.cid.clone());
			out.push(serde_json::to_string(&api::ServerMessage::Ok(ok)).unwrap());
			// Steering gates removed: user messages no longer drive model/metric type or existing/new choices.
			// NOTE: For production WS, `type:"user"` is fast-pathed via `process_user` so we can stream progress.
			// This fallback keeps behavior for direct `handle_message` callers but does not stream progress.
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
			// run agent for this thread using the user text
			tracing::info!("user auto-resume (non-streaming fallback): thread_id={} agent={}", thread_id, agent);
			let frames = run_user_and_frames(&thread_id, &text, &suite_id, &agent, &state.reg, &state.suite_ctx).await?;
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
							let agent_label = state
								.current_agent
								.get(&thread_id)
								.cloned()
								.unwrap_or_else(|| "ask".to_string());
							let meta_val: Option<Value> = rr
								.meta
								.as_ref()
								.and_then(|m| serde_json::to_value(m).ok());
							let _ = store
								.append_step(
									&thread_id,
									ThreadStep::ReviewResponse {
										text: text.clone(),
										meta: meta_val,
										observation: Observation::ok(),
										ts: chrono::Utc::now().to_rfc3339(),
										agent: agent_label,
									},
								)
								.await;
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
						// Log entire thread on final (best-effort)
						if let Ok(log) = store.get(&thread_id).await {
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
			let (messages, next_before) = build_history(&store, &thread_id, req.before_thread_seq, req.limit).await?;
			let mut resp = api::HistoryResponse::new(1, m::history_response::Type::History, now_iso(), state.next_seq(), thread_id.clone(), messages);
			let log = store.get(&thread_id).await?;
			let (suite_id, agent_type) = derive_thread_context(&log);
			resp.title = log.title;
			resp.suite_id = Some(suite_id);
			resp.agent_type = Some(agent_type);
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
			let log = store.get(&thread_id).await?;
			let (_max_assistant, after_seen) = compute_unread_for_log(&log, req.up_to_thread_seq);
			unread = after_seen;
			let resp = api::ServerMessage::Unread(api::UnreadResponse::new(1, m::unread_response::Type::Unread, now_iso(), state.next_seq(), thread_id.clone(), unread));
			let s = serde_json::to_string(&resp).unwrap();
			state.buffer_last(&s);
			out.push(s);
		}
		"plan" => {
			let req: api::PlanRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let thread_id = req.thread_id.clone();
			if thread_id.is_empty() { return Err("thread_id required".into()); }
			if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }

			let snap = load_active_plan_snapshot(&state.suite_ctx, &thread_id).await;
			match snap {
				Some(plan) => {
					let mut resp = api::PlanResponse::new(
						1,
						m::plan_response::Type::Plan,
						now_iso(),
						state.next_seq(),
						thread_id.clone(),
						plan,
					);
					resp.for_cid = Some(req.cid.clone());
					let s = serde_json::to_string(&resp).unwrap();
					state.buffer_last(&s);
					out.push(s);
				}
				None => {
					let mut err = api::ErrorResponse::new(
						1,
						m::error_response::Type::Error,
						now_iso(),
						"no active data_engineer plan for thread".to_string(),
					);
					err.code = Some("not_found".to_string());
					err.cid = Some(req.cid.clone());
					// Hint for clients: this is not transient; avoid tight retry loops.
					err.retry_after_ms = Some(60_000);
					out.push(serde_json::to_string(&err).unwrap());
				}
			}
		}
		"delete" => {
			let req: api::DeleteRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
			let cid = req.cid.clone();
			let thread_id = req.thread_id.clone();
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
			ok.cid = Some(cid.clone());
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
	let agent = match step {
		ThreadStep::SwitchSuite { agent, .. }
		| ThreadStep::SwitchAgent { agent, .. }
		| ThreadStep::User { agent, .. }
		| ThreadStep::Tool { agent, .. }
		| ThreadStep::Phase { agent, .. }
		| ThreadStep::GuardBlock { agent, .. }
		| ThreadStep::ArtifactFocus { agent, .. }
		| ThreadStep::ArtifactSaved { agent, .. }
		| ThreadStep::AskUser { agent, .. }
		| ThreadStep::AskApproval { agent, .. }
		| ThreadStep::ReviewResponse { agent, .. }
		| ThreadStep::Final { agent, .. } => agent.as_str(),
	};
	let raw = serde_json::to_string(step).unwrap_or_else(|_| "{\"type\":\"unknown\"}".to_string());
	let raw = truncate_str(&raw, 220);
	format!("ts={} agent={} step={}", step.ts(), agent, raw)
}

fn derive_current_phase_from_steps(steps: &[ThreadStep]) -> String {
	for step in steps.iter().rev() {
		if let ThreadStep::Phase { phase, .. } = step {
			let t = phase.trim();
			if !t.is_empty() {
				return t.to_string();
			}
		}
	}
	"preflight".to_string()
}

fn derive_completed_phases(order: &[String], current: &str) -> Vec<String> {
	let mut completed: Vec<String> = Vec::new();
	if order.is_empty() {
		return completed;
	}
	let mut idx: Option<usize> = None;
	for (i, p) in order.iter().enumerate() {
		if p == current {
			idx = Some(i);
			break;
		}
	}
	let upto = idx.unwrap_or(0);
	for p in order.iter().take(upto) {
		completed.push(p.clone());
	}
	completed
}

async fn log_thread_steps_if_enabled(store: &ThreadStore, thread_id: &str, reason: &str) {
	if !env_truthy("REACT_LOG_THREAD_STEPS") {
		return;
	}
	match store.get(thread_id).await {
		Ok(log) => {
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
		Err(e) => tracing::info!("THREAD_LOG {} thread_id={} (failed to load: {})", reason, thread_id, e),
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
		move || llm2.chat(&[crate::llm::ChatMessage { role: "user".into(), content: p }])
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
	trace_pref: HashMap<String, bool>,
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
			trace_pref: HashMap::new(),
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

fn trace_enabled_for_thread(state: &mut ConnState, thread_id: &str, explicit: Option<bool>) -> bool {
	if let Some(v) = explicit {
		state.trace_pref.insert(thread_id.to_string(), v);
		return v;
	}
	state.trace_pref.get(thread_id).copied().unwrap_or(false)
}

fn derive_thread_context(log: &ThreadLog) -> (String, String) {
	// Defaults for back-compat threads with no recorded context.
	let mut suite_id = "data_engineer".to_string();
	let mut agent_type = "ask".to_string();
	for step in log.steps.iter() {
		match step {
			ThreadStep::SwitchSuite { to, .. } => {
				if !to.trim().is_empty() {
					suite_id = to.to_string();
				}
			}
			ThreadStep::SwitchAgent { to, .. } => {
				if !to.trim().is_empty() {
					agent_type = to.to_string();
				}
			}
			_ => {}
		}
	}
	(suite_id, agent_type)
}

fn build_suites_catalog(reg: &SuiteRegistry) -> Vec<api::SuitesResponseSuitesInner> {
	let mut out: Vec<api::SuitesResponseSuitesInner> = Vec::new();
	for id in reg.list_ids() {
		match id {
			"data_engineer" => {
				let mut s = api::SuitesResponseSuitesInner::new(
					"data_engineer".to_string(),
					vec!["ask".to_string(), "agent".to_string(), "review".to_string()],
				);
				s.label = Some("Data Engineer".to_string());
				s.default_agent_type = Some("ask".to_string());
				out.push(s);
			}
			"kb" => {
				let mut s = api::SuitesResponseSuitesInner::new("kb".to_string(), vec!["kb".to_string()]);
				s.label = Some("KB".to_string());
				s.default_agent_type = Some("kb".to_string());
				out.push(s);
			}
			_ => {
				let mut s = api::SuitesResponseSuitesInner::new(id.to_string(), vec!["ask".to_string()]);
				s.default_agent_type = Some("ask".to_string());
				out.push(s);
			}
		};
	}
	out
}

fn map_plan_status(s: de_plan::PlanStatus) -> api::PlanStatus {
	match s {
		de_plan::PlanStatus::Draft => api::PlanStatus::Draft,
		de_plan::PlanStatus::Approved => api::PlanStatus::Approved,
		de_plan::PlanStatus::Completed => api::PlanStatus::Completed,
		de_plan::PlanStatus::Cancelled => api::PlanStatus::Cancelled,
	}
}

fn map_task_status(s: de_plan::TaskStatus) -> api::PlanTaskStatus {
	match s {
		de_plan::TaskStatus::Pending => api::PlanTaskStatus::Pending,
		de_plan::TaskStatus::InProgress => api::PlanTaskStatus::InProgress,
		de_plan::TaskStatus::Done => api::PlanTaskStatus::Done,
		de_plan::TaskStatus::Blocked => api::PlanTaskStatus::Blocked,
		de_plan::TaskStatus::NeedsUpdate => api::PlanTaskStatus::NeedsUpdate,
	}
}

async fn load_active_plan_snapshot(ctx: &SuiteCtx, thread_id: &str) -> Option<api::PlanSnapshot> {
	// Plans are stored alongside other top-level resources (threads/, dbt/, etc),
	// NOT under dbt/.
	let base = ctx
		.keyspace
		.threads_prefix(&ctx.scope)
		.trim_end_matches("/threads")
		.trim_end_matches('/')
		.to_string();
	let pref = format!("{}/plans/{}/", base, thread_id.trim());
	let mut keys = ctx.storage.list_prefix(&pref).await.unwrap_or_default();
	keys.sort();

	// Prefer the oldest non-terminal plan (matches suite behavior). If none exist, fall back to the
	// newest terminal plan so UIs can still render the last known plan state (completed/cancelled).
	let mut newest_terminal_cleanse: Option<de_plan::CleansePlan> = None;
	for k in keys.iter().filter(|k| k.ends_with("_cleanse.json")) {
		if let Ok(bytes) = ctx.storage.get_bytes(k).await {
			if let Ok(mut p) = serde_json::from_slice::<de_plan::CleansePlan>(&bytes) {
				if p.plan_key.trim().is_empty() {
					p.plan_key = k.to_string();
				}
				if !p.status.is_terminal() {
					let tasks = p
						.tasks
						.into_iter()
						.map(|t| {
							let mut snap = api::CleanseTaskSnapshot::new(
								t.dataset_id.clone(),
								t.dataset_id.clone(),
								map_task_status(t.status),
							);
							snap.expected_model_path = t.expected_model_path;
							if !t.invariants.is_empty() {
								snap.invariants = Some(t.invariants);
							}
							if !t.notes.is_empty() {
								snap.notes = Some(t.notes);
							}
							api::PlanTask::Cleanse(snap)
						})
						.collect::<Vec<_>>();
					return Some(api::PlanSnapshot::new(
						api::plan_snapshot::PlanKind::Cleanse,
						p.plan_key,
						map_plan_status(p.status),
						tasks,
					));
				} else {
					newest_terminal_cleanse = Some(p);
				}
			}
		}
	}

	let mut newest_terminal_model: Option<de_plan::ModelPlan> = None;
	for k in keys.iter().filter(|k| k.ends_with("_model.json")) {
		if let Ok(bytes) = ctx.storage.get_bytes(k).await {
			if let Ok(mut p) = serde_json::from_slice::<de_plan::ModelPlan>(&bytes) {
				if p.plan_key.trim().is_empty() {
					p.plan_key = k.to_string();
				}
				if !p.status.is_terminal() {
					let tasks = p
						.tasks
						.into_iter()
						.map(|t| {
							let mut snap = api::ModelTaskSnapshot::new(
								t.name.clone(),
								t.name.clone(),
								map_task_status(t.status),
							);
							if !t.folder.trim().is_empty() {
								snap.folder = Some(t.folder);
							}
							if !t.goal.trim().is_empty() {
								snap.goal = Some(t.goal);
							}
							if !t.inputs.is_empty() {
								snap.inputs = Some(t.inputs);
							}
							snap.expected_model_path = t.expected_model_path;
							if !t.invariants.is_empty() {
								snap.invariants = Some(t.invariants);
							}
							if !t.notes.is_empty() {
								snap.notes = Some(t.notes);
							}
							api::PlanTask::Model(snap)
						})
						.collect::<Vec<_>>();
					return Some(api::PlanSnapshot::new(
						api::plan_snapshot::PlanKind::Model,
						p.plan_key,
						map_plan_status(p.status),
						tasks,
					));
				} else {
					newest_terminal_model = Some(p);
				}
			}
		}
	}

	// No active plan. Fall back to the newest terminal plan (if any).
	let choose_terminal = match (&newest_terminal_cleanse, &newest_terminal_model) {
		(Some(c), Some(m)) => {
			// Pick whichever has the lexicographically larger key (timestamped keys sort naturally).
			if c.plan_key >= m.plan_key { "cleanse" } else { "model" }
		}
		(Some(_), None) => "cleanse",
		(None, Some(_)) => "model",
		(None, None) => return None,
	};
	match choose_terminal {
		"cleanse" => {
			let p = newest_terminal_cleanse?;
			let tasks = p
				.tasks
				.into_iter()
				.map(|t| {
					let mut snap = api::CleanseTaskSnapshot::new(
						t.dataset_id.clone(),
						t.dataset_id.clone(),
						map_task_status(t.status),
					);
					snap.expected_model_path = t.expected_model_path;
					if !t.invariants.is_empty() {
						snap.invariants = Some(t.invariants);
					}
					if !t.notes.is_empty() {
						snap.notes = Some(t.notes);
					}
					api::PlanTask::Cleanse(snap)
				})
				.collect::<Vec<_>>();
			Some(api::PlanSnapshot::new(
				api::plan_snapshot::PlanKind::Cleanse,
				p.plan_key,
				map_plan_status(p.status),
				tasks,
			))
		}
		"model" => {
			let p = newest_terminal_model?;
			let tasks = p
				.tasks
				.into_iter()
				.map(|t| {
					let mut snap = api::ModelTaskSnapshot::new(
						t.name.clone(),
						t.name.clone(),
						map_task_status(t.status),
					);
					if !t.folder.trim().is_empty() {
						snap.folder = Some(t.folder);
					}
					if !t.goal.trim().is_empty() {
						snap.goal = Some(t.goal);
					}
					if !t.inputs.is_empty() {
						snap.inputs = Some(t.inputs);
					}
					snap.expected_model_path = t.expected_model_path;
					if !t.invariants.is_empty() {
						snap.invariants = Some(t.invariants);
					}
					if !t.notes.is_empty() {
						snap.notes = Some(t.notes);
					}
					api::PlanTask::Model(snap)
				})
				.collect::<Vec<_>>();
			Some(api::PlanSnapshot::new(
				api::plan_snapshot::PlanKind::Model,
				p.plan_key,
				map_plan_status(p.status),
				tasks,
			))
		}
		_ => None,
	}
}

fn task_id_status_notes(t: &api::PlanTask) -> (String, api::PlanTaskStatus, Vec<String>) {
	match t {
		api::PlanTask::Cleanse(c) => (c.task_id.clone(), c.status, c.notes.clone().unwrap_or_default()),
		api::PlanTask::Model(mo) => (mo.task_id.clone(), mo.status, mo.notes.clone().unwrap_or_default()),
	}
}

fn diff_plan_snapshots(prev: &api::PlanSnapshot, next: &api::PlanSnapshot) -> Vec<api::PlanChange> {
	let mut out: Vec<api::PlanChange> = Vec::new();

	if prev.status != next.status {
		let ch = api::PlanStatusChangedChange::new(prev.status, next.status);
		out.push(api::PlanChange::PlanStatusChanged(ch));
	}

	let mut prev_map: std::collections::HashMap<String, (api::PlanTaskStatus, Vec<String>)> =
		std::collections::HashMap::new();
	for t in prev.tasks.iter() {
		let (id, st, notes) = task_id_status_notes(t);
		prev_map.insert(id, (st, notes));
	}

	for t in next.tasks.iter() {
		let (id, st, notes) = task_id_status_notes(t);
		if let Some((prev_st, prev_notes)) = prev_map.get(&id) {
			if *prev_st != st {
				let ch = api::TaskStatusChangedChange::new(id.clone(), *prev_st, st);
				out.push(api::PlanChange::TaskStatusChanged(ch));
			}
			for n in notes.iter() {
				if !prev_notes.iter().any(|p| p.trim() == n.trim()) {
					let ch = api::TaskNoteAddedChange::new(id.clone(), n.clone());
					out.push(api::PlanChange::TaskNoteAdded(ch));

					let lower = n.to_lowercase();
					let looks_like_error =
						lower.contains("failed") || lower.contains("error") || lower.contains("exception");
					if looks_like_error {
						let err = api::TaskErrorChange::new(id.clone(), n.clone());
						out.push(api::PlanChange::TaskError(err));
					}
				}
			}
		} else {
			let ch = api::TaskStatusChangedChange::new(id.clone(), api::PlanTaskStatus::Pending, st);
			out.push(api::PlanChange::TaskStatusChanged(ch));
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
	let agent = normalize_agent_new(req.agent_type)?;
	let trace_enabled = req.trace.unwrap_or(false);
	state.current_suite.insert(thread_id.clone(), suite_id.clone());
	state.current_agent.insert(thread_id.clone(), agent.clone());
	// Persist initial suite/agent selection so it survives reconnects
	{
		let store = state.thread_store();
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::SwitchSuite {
					from: None,
					to: suite_id.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::SwitchAgent {
					from: None,
					to: agent.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
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
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::User {
					text: question.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
		let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
	}
	run_agent_with_processing(&thread_id, &question, &suite_id, &agent, &cid, trace_enabled, true, state, write, m::processing_response::Stage::Queued).await
}

async fn process_open(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let req: api::OpenRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
	let cid = req.cid.clone();
	let thread_id = req.thread_id.clone();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	let question = req.question.clone().unwrap_or_else(|| "Continue.".to_string());
	let requested_suite = req.suite_id.clone();
	let requested_agent = normalize_agent_open(req.agent_type)?;
	let trace_enabled = trace_enabled_for_thread(state, &thread_id, req.trace);

	// Derive current suite/agent from persisted thread log (durable across reconnects)
	let store = state.thread_store();
	let log = store.get(&thread_id).await?;
	let (current_suite, current_agent) = derive_thread_context(&log);
	if current_suite != requested_suite {
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::SwitchSuite {
					from: Some(current_suite.clone()),
					to: requested_suite.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: requested_agent.clone(),
				},
			)
			.await;
	}
	state.current_suite.insert(thread_id.clone(), requested_suite.clone());

	if current_agent != requested_agent {
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::SwitchAgent {
					from: Some(current_agent.clone()),
					to: requested_agent.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: requested_agent.clone(),
				},
			)
			.await;
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
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::User {
					text: question.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
		let _ = store.set_title_if_absent(&thread_id, &truncate_title(&question, 64)).await;
	}
	run_agent_with_processing(&thread_id, &question, &suite_id, &agent, &cid, trace_enabled, false, state, write, m::processing_response::Stage::Queued).await
}

async fn process_user(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let req: api::UserRequest = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
	let cid = req.cid.clone();
	let thread_id = req.thread_id.clone();
	let text = req.text.clone();
	if thread_id.is_empty() || text.trim().is_empty() { return Err("thread_id and text required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }

	// Reconnect-safe: derive suite/agent from persisted history if not present in connection state
	if !state.current_suite.contains_key(&thread_id) || !state.current_agent.contains_key(&thread_id) {
		let store = state.thread_store();
		let log = store.get(&thread_id).await?;
		let (suite_id, agent_type) = derive_thread_context(&log);
		state.current_suite.insert(thread_id.clone(), suite_id);
		state.current_agent.insert(thread_id.clone(), agent_type);
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
	let trace_enabled = trace_enabled_for_thread(state, &thread_id, req.trace);

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

	// record user message
	{
		let store = state.thread_store();
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::User {
					text: text.clone(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
	}
	run_agent_with_processing_suite(
		&thread_id,
		&text,
		&suite_id,
		&agent,
		&cid,
		trace_enabled,
		SuiteRunKind::User,
		state,
		write,
		m::processing_response::Stage::Queued,
	)
	.await
}

async fn process_approve(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let cid = v.get("cid").and_then(|x| x.as_str()).ok_or_else(|| "cid required".to_string())?.to_string();
	let thread_id = v.get("thread_id").and_then(|x| x.as_str()).ok_or_else(|| "thread_id required".to_string())?.to_string();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	// Reconnect-safe: derive suite/agent from persisted history if not present in connection state
	if !state.current_suite.contains_key(&thread_id) || !state.current_agent.contains_key(&thread_id) {
		let store = state.thread_store();
		let log = store.get(&thread_id).await?;
		let (suite_id, agent_type) = derive_thread_context(&log);
		state.current_suite.insert(thread_id.clone(), suite_id);
		state.current_agent.insert(thread_id.clone(), agent_type);
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
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::User {
					text: "approve".to_string(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
	}
	tracing::info!("approve: thread_id={} agent={}", thread_id, agent);
	run_agent_with_processing(&thread_id, "Continue.", &suite_id, &agent, &cid, false, false, state, write, m::processing_response::Stage::Queued).await
}

async fn process_reject(v: &Value, state: &mut ConnState, write: &mut (impl SinkExt<Message> + Unpin)) -> Result<(), String> {
	let cid = v.get("cid").and_then(|x| x.as_str()).ok_or_else(|| "cid required".to_string())?.to_string();
	let thread_id = v.get("thread_id").and_then(|x| x.as_str()).ok_or_else(|| "thread_id required".to_string())?.to_string();
	if thread_id.is_empty() { return Err("thread_id required".into()); }
	if uuid::Uuid::parse_str(&thread_id).is_err() { return Err("invalid thread_id".into()); }
	// Reconnect-safe: derive suite/agent from persisted history if not present in connection state
	if !state.current_suite.contains_key(&thread_id) || !state.current_agent.contains_key(&thread_id) {
		let store = state.thread_store();
		let log = store.get(&thread_id).await?;
		let (suite_id, agent_type) = derive_thread_context(&log);
		state.current_suite.insert(thread_id.clone(), suite_id);
		state.current_agent.insert(thread_id.clone(), agent_type);
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
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::User {
					text: "reject".to_string(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: agent.clone(),
				},
			)
			.await;
	}
	tracing::info!("reject: thread_id={} agent={}", thread_id, agent);
	run_agent_with_processing(&thread_id, "Continue.", &suite_id, &agent, &cid, false, false, state, write, m::processing_response::Stage::Queued).await
}
// normalize_agent removed (unused)

fn normalize_agent_new(a: api::new_request::AgentType) -> Result<String, String> {
	match a {
		api::new_request::AgentType::Ask => Ok("ask".to_string()),
		api::new_request::AgentType::Agent => Ok("agent".to_string()),
		api::new_request::AgentType::Review => Ok("review".to_string()),
		api::new_request::AgentType::Kb => Ok("kb".to_string()),
		// No legacy modes: remove cleanse/model from WS-facing agent types.
		api::new_request::AgentType::Cleanse | api::new_request::AgentType::Model => Err(
			"agent_type 'cleanse'/'model' is no longer supported; use agent_type='agent' and let phases handle cleanse vs model."
				.to_string(),
		),
	}
}

fn normalize_agent_open(a: api::open_request::AgentType) -> Result<String, String> {
	match a {
		api::open_request::AgentType::Ask => Ok("ask".to_string()),
		api::open_request::AgentType::Agent => Ok("agent".to_string()),
		api::open_request::AgentType::Review => Ok("review".to_string()),
		api::open_request::AgentType::Kb => Ok("kb".to_string()),
		api::open_request::AgentType::Cleanse | api::open_request::AgentType::Model => Err(
			"agent_type 'cleanse'/'model' is no longer supported; use agent_type='agent' and let phases handle cleanse vs model."
				.to_string(),
		),
	}
}
async fn run_agent_with_processing(
	thread_id: &str,
	question: &str,
	suite_id: &str,
	agent: &str,
	cid: &str,
	trace_enabled: bool,
	is_new: bool,
	state: &mut ConnState,
	write: &mut (impl SinkExt<Message> + Unpin),
	initial_stage: m::processing_response::Stage,
) -> Result<(), String> {
	// Suite-based runner. We intentionally keep this simple: the suite owns prompts/tools and the core
	// WS server only handles message I/O. Step-level progress streaming can be reintroduced later by
	// threading progress channels through the suite runner.
	let kind = if is_new { SuiteRunKind::New } else { SuiteRunKind::Open };
	return run_agent_with_processing_suite(thread_id, question, suite_id, agent, cid, trace_enabled, kind, state, write, initial_stage).await;
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

fn trace_line_to_text(line: &str) -> Option<String> {
	// Avoid leaking prompts/tool-cards/system text; only stream toolcall/observation summaries.
	let s = line.trim();
	if s.starts_with("System:") || s.starts_with("Tools:") || s.starts_with("User:") {
		return None;
	}
	if let Some(rest) = s.strip_prefix("Assistant:") {
		// Try to parse the action JSON and emit a compact tool call line.
		if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest.trim()) {
			if let Some(action) = v.get("action").and_then(|x| x.as_str()) {
				return Some(format!("tool_call {action}"));
			}
			if v.get("final").is_some() {
				return None;
			}
		}
		return None;
	}
	if let Some(rest) = s.strip_prefix("Observation:") {
		if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest.trim()) {
			let ok = v.get("ok").and_then(|x| x.as_bool());
			let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
			return Some(match ok {
				Some(true) => "tool_ok".to_string(),
				Some(false) => {
					if err.is_empty() { "tool_error".to_string() } else { format!("tool_error: {err}") }
				}
				None => "tool_observation".to_string(),
			});
		}
		return Some("tool_observation".to_string());
	}
	None
}
enum AgentFrame {
	Final { answer: String, sql: Option<String> },
	Review { text: String, meta: Option<serde_json::Value> },
	AwaitUser { prompt: String },
	AwaitApproval { prompt: String },
}

#[derive(Clone, Copy, Debug)]
enum SuiteRunKind {
	New,
	Open,
	User,
}

async fn run_agent_with_processing_suite(
	thread_id: &str,
	question: &str,
	suite_id: &str,
	agent: &str,
	cid: &str,
	trace_enabled: bool,
	kind: SuiteRunKind,
	state: &mut ConnState,
	write: &mut (impl SinkExt<Message> + Unpin),
	_initial_stage: m::processing_response::Stage,
) -> Result<(), String> {
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
	let suite_for_progress = suite.clone();
	let thread_id_s = thread_id.to_string();
	let q_s = question.to_string();
	let agent_s = agent.to_string();

	let agent_task = tokio::spawn(async move {
		match kind {
			SuiteRunKind::New => suite.handle_new(&thread_id_s, &q_s, &agent_s, &sctx2).await,
			SuiteRunKind::Open => suite.handle_open(&thread_id_s, &q_s, &agent_s, &sctx2).await,
			SuiteRunKind::User => suite.handle_user(&thread_id_s, &q_s, &agent_s, &sctx2).await,
		}
	});
	tokio::pin!(agent_task);

	// Emit suite phase/progress frames while the suite is running (best-effort polling).
	// This is intentionally cheap and decoupled: the suite already records `phase` steps in the thread log.
	let phases_order = suite_for_progress.phase_order(agent);
	let mut last_phase_sent: Option<String> = None;
	let mut phase_tick = tokio::time::interval(std::time::Duration::from_millis(250));
	phase_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

	// Emit plan_update frames for data_engineer while the suite is running.
	let plan_updates_enabled = suite_id == "data_engineer";
	let mut last_plan_sent: Option<api::PlanSnapshot> = None;
	let mut plan_tick = tokio::time::interval(std::time::Duration::from_millis(400));
	plan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

	// While the suite/agent is running, forward trace lines (if enabled).
	loop {
		tokio::select! {
			_ = phase_tick.tick() => {
				if phases_order.is_empty() {
					continue;
				}
				let store = state.thread_store();
				let phase = match store.get(thread_id).await {
					Ok(log) => derive_current_phase_from_steps(&log.steps),
					Err(_) => "preflight".to_string(),
				};
				if last_phase_sent.as_deref() == Some(phase.as_str()) {
					continue;
				}
				last_phase_sent = Some(phase.clone());
				let completed = derive_completed_phases(&phases_order, &phase);
				let mut sp = api::SuiteProgressResponse::new(
					1,
					m::suite_progress_response::Type::SuiteProgress,
					now_iso(),
					state.next_seq(),
					thread_id.to_string(),
					suite_id.to_string(),
					phases_order.clone(),
					completed,
					phase,
				);
				sp.for_cid = Some(cid.to_string());
				let s = serde_json::to_string(&sp).unwrap();
				state.buffer_last(&s);
				tracing::info!("WS -> {}", s);
				let _ = write.send(Message::Text(s)).await;
			}
			_ = plan_tick.tick() => {
				if !plan_updates_enabled {
					continue;
				}
				let next = load_active_plan_snapshot(&state.suite_ctx, thread_id).await;
				let should_emit = match (&last_plan_sent, &next) {
					(None, Some(_)) => true,
					(Some(_), None) => true,
					(Some(a), Some(b)) => a != b,
					(None, None) => false,
				};
				if !should_emit {
					continue;
				}
				let changes = match (&last_plan_sent, &next) {
					(Some(prev), Some(cur)) => diff_plan_snapshots(prev, cur),
					_ => Vec::new(),
				};
				last_plan_sent = next.clone();
				if let Some(plan) = next {
					let mut pu = api::PlanUpdateResponse::new(
						1,
						m::plan_update_response::Type::PlanUpdate,
						now_iso(),
						state.next_seq(),
						thread_id.to_string(),
						plan,
						changes,
					);
					pu.for_cid = Some(cid.to_string());
					let s = serde_json::to_string(&pu).unwrap();
					state.buffer_last(&s);
					tracing::info!("WS -> {}", s);
					let _ = write.send(Message::Text(s)).await;
				}
			}
			Some(line) = trace_rx.recv() => {
				if !trace_enabled { continue; }
				let Some(mut text) = trace_line_to_text(&line) else { continue };
				if text.len() > 500 {
					text = format!("{}…", &text[..500]);
				}
				let mut tr = api::TraceResponse::new(
					1,
					m::trace_response::Type::Trace,
					now_iso(),
					state.next_seq(),
					thread_id.to_string(),
					text,
				);
				tr.for_cid = Some(cid.to_string());
				let s = serde_json::to_string(&tr).unwrap();
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
										ThreadStep::ReviewResponse {
											text: text.clone(),
											meta: resp.meta.as_ref().and_then(|m| serde_json::to_value(m).ok()),
											observation: Observation::ok(),
											ts: chrono::Utc::now().to_rfc3339(),
											agent: agent.to_string(),
										}
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

async fn run_user_and_frames(
	thread_id: &str,
	text: &str,
	suite_id: &str,
	agent: &str,
	reg: &SuiteRegistry,
	sctx: &SuiteCtx,
) -> Result<Vec<AgentFrame>, String> {
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
		.handle_user(thread_id, text, agent, sctx)
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
		match step {
			ThreadStep::User { .. } => {
				tseq += 1;
			}
			ThreadStep::Final { .. }
			| ThreadStep::AskUser { .. }
			| ThreadStep::AskApproval { .. }
			| ThreadStep::ReviewResponse { .. } => {
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

async fn build_history(
	store: &ThreadStore,
	thread_id: &str,
	before: Option<i32>,
	limit_opt: Option<i32>,
) -> Result<(Vec<api::HistoryResponseMessagesInner>, Option<i32>), String> {
	let mut msgs: Vec<api::HistoryResponseMessagesInner> = Vec::new();
	let mut next_before: Option<i32> = None;
	let limit = limit_opt.unwrap_or(50).max(1);
	let log = store.get(thread_id).await?;
	let mut tseq: i32 = 0;
	let mut all_msgs: Vec<api::HistoryResponseMessagesInner> = Vec::new();
	for step in log.steps.iter() {
		match step {
			ThreadStep::User { text, .. } => {
				tseq += 1;
				all_msgs.push(api::HistoryResponseMessagesInner {
					thread_seq: tseq,
					role: m::history_response_messages_inner::Role::User,
					content: text.to_string(),
					created_at: step.ts().to_string(),
				});
			}
			ThreadStep::Final { answer, .. } => {
				tseq += 1;
				all_msgs.push(api::HistoryResponseMessagesInner {
					thread_seq: tseq,
					role: m::history_response_messages_inner::Role::Assistant,
					content: answer.to_string(),
					created_at: step.ts().to_string(),
				});
			}
			ThreadStep::ReviewResponse { text, .. } => {
				tseq += 1;
				all_msgs.push(api::HistoryResponseMessagesInner {
					thread_seq: tseq,
					role: m::history_response_messages_inner::Role::Assistant,
					content: text.to_string(),
					created_at: step.ts().to_string(),
				});
			}
			ThreadStep::AskUser { prompt, .. } => {
				tseq += 1;
				all_msgs.push(api::HistoryResponseMessagesInner {
					thread_seq: tseq,
					role: m::history_response_messages_inner::Role::Assistant,
					content: prompt.to_string(),
					created_at: step.ts().to_string(),
				});
			}
			ThreadStep::AskApproval { prompt, .. } => {
				tseq += 1;
				all_msgs.push(api::HistoryResponseMessagesInner {
					thread_seq: tseq,
					role: m::history_response_messages_inner::Role::Assistant,
					content: prompt.to_string(),
					created_at: step.ts().to_string(),
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
	Ok((msgs, next_before))
}

#[cfg(test)]
mod tests {
	use super::*;
	use async_trait::async_trait;
	use futures_util::sink::Sink;
	use react_core::storage::InMemoryStorageAdapter;
	use react_core::keyspace::Keyspace;
	use react_core::providers::NullSecretsProvider;
	use react_core::llm::NullModel;
	use crate::providers::{DefaultKeyspace, RequestScope};
	use serde_json::json;
	use std::sync::Arc;
	use std::sync::Mutex;
	use std::pin::Pin;
	use std::task::{Context, Poll};
	use std::time::Duration;

	#[derive(Clone, Default)]
	struct CollectSink {
		out: Arc<Mutex<Vec<String>>>,
	}

	impl Sink<Message> for CollectSink {
		type Error = std::convert::Infallible;

		fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
			Poll::Ready(Ok(()))
		}

		fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
			if let Message::Text(s) = item {
				self.out.lock().unwrap().push(s);
			}
			Ok(())
		}

		fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
			Poll::Ready(Ok(()))
		}

		fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
			Poll::Ready(Ok(()))
		}
	}

	struct StubDataEngineerSuite;

	#[async_trait]
	impl react_suites::suite::Suite for StubDataEngineerSuite {
		fn id(&self) -> &'static str {
			"data_engineer"
		}

		fn phase_order(&self, _agent_type: &str) -> Vec<String> {
			vec!["preflight".to_string(), "done".to_string()]
		}

		async fn handle_new(
			&self,
			_thread_id: &str,
			_question: &str,
			_agent_type: &str,
			_ctx: &SuiteCtx,
		) -> Result<Vec<react_suites::FlowFrame>, String> {
			Ok(vec![react_suites::FlowFrame::Final { answer: "ok".to_string(), sql: Some("SELECT 1".to_string()) }])
		}

		async fn handle_open(
			&self,
			_thread_id: &str,
			_question: &str,
			_agent_type: &str,
			_ctx: &SuiteCtx,
		) -> Result<Vec<react_suites::FlowFrame>, String> {
			Ok(vec![react_suites::FlowFrame::Final { answer: "ok".to_string(), sql: Some("SELECT 1".to_string()) }])
		}

		async fn handle_user(
			&self,
			_thread_id: &str,
			_text: &str,
			_agent_type: &str,
			ctx: &SuiteCtx,
		) -> Result<Vec<react_suites::FlowFrame>, String> {
			if let Some(tx) = ctx.trace_tx.as_ref() {
				let _ = tx.send("Assistant: {\"action\":\"dbt_files\",\"args\":{}}".to_string());
				let _ = tx.send("Observation: {\"ok\":true}".to_string());
			}
			// Sleep long enough for phase_tick (250ms) and plan_tick (400ms) to emit at least once.
			tokio::time::sleep(Duration::from_millis(520)).await;
			Ok(vec![react_suites::FlowFrame::Final { answer: "ok".to_string(), sql: Some("SELECT 1".to_string()) }])
		}
	}

	#[test]
	fn normalize_agent_includes_agent_and_review() {
		assert_eq!(normalize_agent_new(api::new_request::AgentType::Agent).unwrap(), "agent");
		assert_eq!(normalize_agent_new(api::new_request::AgentType::Review).unwrap(), "review");
		assert_eq!(normalize_agent_open(api::open_request::AgentType::Agent).unwrap(), "agent");
		assert_eq!(normalize_agent_open(api::open_request::AgentType::Review).unwrap(), "review");
		assert!(normalize_agent_new(api::new_request::AgentType::Cleanse).is_err());
		assert!(normalize_agent_new(api::new_request::AgentType::Model).is_err());
		assert!(normalize_agent_open(api::open_request::AgentType::Cleanse).is_err());
		assert!(normalize_agent_open(api::open_request::AgentType::Model).is_err());
	}

	#[test]
	fn derive_thread_context_ignores_step_agent_labels() {
		let log = ThreadLog {
			steps: vec![
				ThreadStep::SwitchSuite {
					from: None,
					to: "data_engineer".to_string(),
					observation: Observation::ok(),
					ts: "t".to_string(),
					agent: "agent".to_string(),
				},
				ThreadStep::SwitchAgent {
					from: None,
					to: "agent".to_string(),
					observation: Observation::ok(),
					ts: "t".to_string(),
					agent: "agent".to_string(),
				},
				// Inner phase/tool steps may record agent labels like "cleanse" — these must NOT
				// override the user-selected agent_type derived from switch_agent.
				ThreadStep::Tool {
					name: "sql_schema".to_string(),
					args: json!({}),
					observation: ToolObservation::normalize(json!({"ok": true})),
					ts: "t".to_string(),
					agent: "cleanse".to_string(),
				},
			],
			..Default::default()
		};
		let (_suite, agent_type) = derive_thread_context(&log);
		assert_eq!(agent_type, "agent");
	}

	#[test]
	fn unread_counts_include_review_response() {
		let log = ThreadLog {
			steps: vec![
				ThreadStep::User {
					text: "hi".to_string(),
					observation: Observation::ok(),
					ts: "t".to_string(),
					agent: "ask".to_string(),
				},
				ThreadStep::ReviewResponse {
					text: "review text".to_string(),
					meta: None,
					observation: Observation::ok(),
					ts: "t".to_string(),
					agent: "agent".to_string(),
				},
				ThreadStep::Final {
					answer: "done".to_string(),
					sql: None,
					observation: Observation::ok(),
					ts: "t".to_string(),
					agent: "agent".to_string(),
				},
			],
			..Default::default()
		};
		// No seen messages yet -> both assistant messages should count as unread.
		let (_max_seq, unread) = compute_unread_for_log(&log, 0);
		assert_eq!(unread, 2);
	}

	#[test]
	fn trace_line_filter_does_not_leak_prompts() {
		assert_eq!(trace_line_to_text("System: secret prompt"), None);
		assert_eq!(trace_line_to_text("Tools: huge tool card"), None);
		assert_eq!(trace_line_to_text("User: hello"), None);

		assert_eq!(
			trace_line_to_text("Assistant: {\"action\":\"run_sql\",\"args\":{\"sql\":\"select 1\"}}").as_deref(),
			Some("tool_call run_sql")
		);

		assert_eq!(
			trace_line_to_text("Observation: {\"ok\":true}").as_deref(),
			Some("tool_ok")
		);
	}

	#[tokio::test]
	async fn history_includes_review_response_as_assistant_message() {
		let storage = Arc::new(InMemoryStorageAdapter::default());
		let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
		let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
		let store = ThreadStore::new(storage, scope, keyspace);

		let tid = "thread1";
		let _ = store
			.append_step(
				tid,
				ThreadStep::User {
					text: "start".to_string(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: "ask".to_string(),
				},
			)
			.await;

		let _ = store
			.append_step(
				tid,
				ThreadStep::ReviewResponse {
					text: "review text".to_string(),
					meta: None,
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: "agent".to_string(),
				},
			)
			.await;

		let (msgs, _next) = build_history(&store, tid, None, Some(50)).await.unwrap();
		assert!(msgs.iter().any(|m| m.role == m::history_response_messages_inner::Role::Assistant && m.content == "review text"));
	}

	#[tokio::test]
	async fn suites_request_requires_cid_and_v() {
		let storage = Arc::new(InMemoryStorageAdapter::default());
		let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
		let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
		let suite_ctx = SuiteCtx::new(
			storage,
			Arc::new(NullSecretsProvider::default()),
			Arc::new(NullModel::new()),
			scope,
			keyspace,
		);
		let reg = Arc::new(react_suites::default_registry());
		let mut state = ConnState::new(reg, suite_ctx);

		// Missing required fields should fail strict parsing.
		let bad = json!({"type":"suites"}).to_string();
		assert!(handle_message(&bad, &mut state).await.is_err());
	}

	#[tokio::test]
	async fn delete_request_requires_cid_and_v() {
		let storage = Arc::new(InMemoryStorageAdapter::default());
		let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
		let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
		let suite_ctx = SuiteCtx::new(
			storage,
			Arc::new(NullSecretsProvider::default()),
			Arc::new(NullModel::new()),
			scope,
			keyspace,
		);
		let reg = Arc::new(react_suites::default_registry());
		let mut state = ConnState::new(reg, suite_ctx);

		let bad = json!({"type":"delete","thread_id":"not-a-uuid"}).to_string();
		assert!(handle_message(&bad, &mut state).await.is_err());
	}

	#[tokio::test]
	async fn plan_request_returns_snapshot_when_plan_exists() {
		let storage = Arc::new(InMemoryStorageAdapter::default());
		let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
		let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
		let suite_ctx = SuiteCtx::new(
			storage.clone(),
			Arc::new(NullSecretsProvider::default()),
			Arc::new(NullModel::new()),
			scope.clone(),
			keyspace.clone(),
		);
		let reg = Arc::new(react_suites::default_registry());
		let mut state = ConnState::new(reg, suite_ctx.clone());

		let thread_id = uuid::Uuid::new_v4().to_string();
		let base = keyspace
			.threads_prefix(&scope)
			.trim_end_matches("/threads")
			.trim_end_matches('/')
			.to_string();
		let plan_key = format!("{}/plans/{}/20260126T000000Z_cleanse.json", base, thread_id);

		let plan = de_plan::CleansePlan {
			plan_key: plan_key.clone(),
			status: de_plan::PlanStatus::Approved,
			project_snapshot: serde_json::json!({}),
			tasks: vec![de_plan::CleanseTask {
				dataset_id: "a.b.c".to_string(),
				expected_model_path: Some("models/staging/stg_a_b_c.sql".to_string()),
				invariants: vec!["pk: id".to_string()],
				status: de_plan::TaskStatus::Pending,
				notes: vec!["initial plan".to_string()],
			}],
			batches: vec![vec!["a.b.c".to_string()]],
			progress: de_plan::PlanProgress::default(),
		};
		let bytes = serde_json::to_vec_pretty(&plan).unwrap();
		suite_ctx.storage.put_bytes(&plan_key, &bytes, "application/json").await.unwrap();

		let msg = json!({"v":1,"type":"plan","cid":"c1","thread_id":thread_id}).to_string();
		let frames = handle_message(&msg, &mut state).await.unwrap();
		assert_eq!(frames.len(), 1);
		let resp: api::PlanResponse = serde_json::from_str(&frames[0]).unwrap();
		assert_eq!(resp.for_cid.as_deref(), Some("c1"));
		assert_eq!(resp.plan.plan_kind, api::plan_snapshot::PlanKind::Cleanse);
		assert_eq!(resp.plan.status, api::PlanStatus::Approved);
	}

	#[tokio::test]
	async fn user_request_streams_suite_progress_plan_update_and_trace() {
		let storage = Arc::new(InMemoryStorageAdapter::default());
		let scope = RequestScope { tenant: "t".to_string(), workspace: "w".to_string(), project_id: "p".to_string() };
		let keyspace = Arc::new(DefaultKeyspace::new("b".to_string()));
		let suite_ctx = SuiteCtx::new(
			storage.clone(),
			Arc::new(NullSecretsProvider::default()),
			Arc::new(NullModel::new()),
			scope.clone(),
			keyspace.clone(),
		);

		// Seed a thread with durable suite/agent selection so process_user can derive context.
		let thread_id = uuid::Uuid::new_v4().to_string();
		let store = ThreadStore::new(storage.clone(), scope.clone(), keyspace.clone());
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::SwitchSuite {
					from: None,
					to: "data_engineer".to_string(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: "agent".to_string(),
				},
			)
			.await;
		let _ = store
			.append_step(
				&thread_id,
				ThreadStep::SwitchAgent {
					from: None,
					to: "agent".to_string(),
					observation: Observation::ok(),
					ts: chrono::Utc::now().to_rfc3339(),
					agent: "agent".to_string(),
				},
			)
			.await;

		// Seed an approved plan so plan_update can be emitted during the run.
		let base = keyspace
			.threads_prefix(&scope)
			.trim_end_matches("/threads")
			.trim_end_matches('/')
			.to_string();
		let plan_key = format!("{}/plans/{}/20260126T000000Z_cleanse.json", base, thread_id);
		let plan = de_plan::CleansePlan {
			plan_key: plan_key.clone(),
			status: de_plan::PlanStatus::Approved,
			project_snapshot: serde_json::json!({}),
			tasks: vec![de_plan::CleanseTask {
				dataset_id: "a.b.c".to_string(),
				expected_model_path: Some("models/staging/stg_a_b_c.sql".to_string()),
				invariants: vec![],
				status: de_plan::TaskStatus::Pending,
				notes: vec![],
			}],
			batches: vec![vec!["a.b.c".to_string()]],
			progress: de_plan::PlanProgress::default(),
		};
		let bytes = serde_json::to_vec_pretty(&plan).unwrap();
		suite_ctx.storage.put_bytes(&plan_key, &bytes, "application/json").await.unwrap();

		let mut reg = react_suites::registry::SuiteRegistry::new();
		reg.register(StubDataEngineerSuite);
		let reg = Arc::new(reg);
		let mut state = ConnState::new(reg, suite_ctx);
		// Inherit trace setting when UserRequest.trace is omitted.
		state.trace_pref.insert(thread_id.clone(), true);

		let msg = json!({"v":1,"type":"user","cid":"c1","thread_id":thread_id,"text":"continue"}); // trace omitted
		let mut sink = CollectSink::default();
		process_user(&msg, &mut state, &mut sink).await.unwrap();

		let frames = sink.out.lock().unwrap().clone();
		assert!(!frames.is_empty());
		let mut saw_suite_progress = false;
		let mut saw_plan_update = false;
		let mut saw_trace = false;
		for s in frames {
			if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
				match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
					"suite_progress" => saw_suite_progress = true,
					"plan_update" => saw_plan_update = true,
					"trace" => saw_trace = true,
					_ => {}
				}
			}
		}
		assert!(saw_suite_progress, "expected suite_progress frame during user-triggered run");
		assert!(saw_plan_update, "expected plan_update frame during user-triggered run");
		assert!(saw_trace, "expected trace frame during user-triggered run");
	}

	#[test]
	fn diff_plan_snapshots_emits_status_and_note_changes() {
		let prev_task = api::CleanseTaskSnapshot::new(
			"a.b.c".to_string(),
			"a.b.c".to_string(),
			api::PlanTaskStatus::Pending,
		);
		let prev = api::PlanSnapshot::new(
			api::plan_snapshot::PlanKind::Cleanse,
			"k".to_string(),
			api::PlanStatus::Approved,
			vec![api::PlanTask::Cleanse(prev_task)],
		);

		let mut next_task = api::CleanseTaskSnapshot::new(
			"a.b.c".to_string(),
			"a.b.c".to_string(),
			api::PlanTaskStatus::Blocked,
		);
		next_task.notes = Some(vec!["staging_model failed: boom".to_string()]);
		let next = api::PlanSnapshot::new(
			api::plan_snapshot::PlanKind::Cleanse,
			"k".to_string(),
			api::PlanStatus::Approved,
			vec![api::PlanTask::Cleanse(next_task)],
		);

		let changes = diff_plan_snapshots(&prev, &next);
		assert!(changes.iter().any(|c| matches!(c, api::PlanChange::TaskStatusChanged(_))));
		assert!(changes.iter().any(|c| matches!(c, api::PlanChange::TaskNoteAdded(_))));
		assert!(changes.iter().any(|c| matches!(c, api::PlanChange::TaskError(_))));
	}
}