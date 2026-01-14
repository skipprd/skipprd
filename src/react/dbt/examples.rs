use serde_json::Value;
use std::path::{Path, PathBuf};
use std::fs;
use tokio::sync::OnceCell;
use tracing::{info, warn, error};
use sha2::{Sha256, Digest};

use crate::react::vector::lance_store::{Chunk, ScoredChunk};
use crate::adapters::storage::StorageAdapter;
use crate::react::providers::{Keyspace, RequestScope};
use std::sync::Arc;

static SYNC_ONCE: OnceCell<()> = OnceCell::const_new();

/// Ensure the global DBT examples are synced once per process startup.
pub async fn ensure_synced_once(
	storage: Arc<dyn StorageAdapter>,
	keyspace: Arc<dyn Keyspace>,
	scope: RequestScope,
	llm: Arc<dyn crate::llm::LargeLanguageModel>,
) {
	let _ = SYNC_ONCE.get_or_init(|| async {
		info!("dbt_examples: ensure_synced_once triggered (startup)");
		match sync_from_repo_to_s3_and_embeddings(storage, keyspace, scope, llm).await {
			Ok(_) => (),
			Err(e) => warn!("DBT examples sync skipped/failed: {}", e),
		}
	}).await;
}

fn local_examples_root() -> PathBuf {
	// Relative to current working directory
	PathBuf::from("dbt-examples")
}

fn include_file(path: &Path) -> bool {
	if !path.is_file() { return false; }
	if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
		// Skip lock/cache dirs/files
		if name.starts_with('.') || name == "target" || name == "__pycache__" { return false; }
	}
	// Skip common repo/system directories anywhere in the path
	for c in path.components() {
		if let std::path::Component::Normal(os) = c {
			if let Some(s) = os.to_str() {
				let s_l = s.to_ascii_lowercase();
				if s_l == ".git" || s_l == ".github" || s_l == ".circleci" || s_l == ".vscode" || s_l == ".idea" || s_l == "node_modules" || s_l == "venv" {
					return false;
				}
			}
		}
	}
	match path.extension().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
		"sql" | "yml" | "yaml" | "md" | "py" => true,
		_ => false,
	}
}

fn sha256_bytes(bytes: &[u8]) -> String {
	let mut hasher = Sha256::new();
	hasher.update(bytes);
	let out = hasher.finalize();
	hex::encode(out)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
struct ProjectFile {
	path: String,
	sha256: String,
	bytes: usize,
	mtime: i64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
struct ProjectManifest {
	project: String,
	files: Vec<ProjectFile>,
	project_digest: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
struct GlobalManifest {
	projects: Vec<ProjectManifest>,
}

fn chunk_text(s: &str, max_chars: usize) -> Vec<String> {
	if s.is_empty() { return vec![]; }
	let mut out = Vec::new();
	let mut i = 0usize;
	let len = s.len();
	while i < len {
		let j = (i + max_chars).min(len);
		out.push(s[i..j].to_string());
		i = j;
	}
	out
}

async fn load_global_manifest(storage: Arc<dyn StorageAdapter>) -> GlobalManifest {
	let key = "dbt-examples/manifest.json";
	match storage.get_json(key).await {
		Ok(v) => serde_json::from_value(v).unwrap_or_default(),
		Err(_) => GlobalManifest::default(),
	}
}

async fn save_global_manifest(storage: Arc<dyn StorageAdapter>, m: &GlobalManifest) {
	let key = "dbt-examples/manifest.json";
	let _ = storage.put_json(key, &serde_json::to_value(m).unwrap_or(Value::Null)).await;
}

fn collect_local_projects(root: &Path) -> Vec<(String, Vec<(PathBuf, Vec<u8>)>)> {
	let mut out: Vec<(String, Vec<(PathBuf, Vec<u8>)>)> = Vec::new();
	if !root.exists() { return out; }
	let rd = match fs::read_dir(root) { Ok(r) => r, Err(_) => return out };
	fn allowed_example_rel_path(rel: &str) -> bool {
		let r = rel.replace('\\', "/");
		r.starts_with("models/")
			|| r.starts_with("metrics/")
			|| r.starts_with("macros/")
			|| r.starts_with("snapshots/")
			|| r.starts_with("seeds/")
			|| r.starts_with("analyses/")
			|| r.starts_with("tests/")
			|| r.starts_with("exposures/")
			|| r.starts_with("docs/")
			|| r == "dbt_project.yml"
			|| r == "packages.yml"
	}
	for entry in rd.flatten() {
		let p = entry.path();
		if p.is_dir() {
			let project = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
			if project.is_empty() { continue; }
			let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
			let mut stack: Vec<PathBuf> = vec![p.clone()];
			while let Some(dir) = stack.pop() {
				if let Ok(rd2) = fs::read_dir(&dir) {
					for e2 in rd2.flatten() {
						let p2 = e2.path();
						if p2.is_dir() {
							stack.push(p2);
						} else if include_file(&p2) {
							let rel = p2.strip_prefix(&p.join(&project)).ok()
								.and_then(|pb| pb.strip_prefix(&p).ok()).unwrap_or(&p2);
							let rel2 = p2.strip_prefix(&p.join(&project)).unwrap_or(&p2);
							let rel_str = rel2.to_string_lossy().to_string();
							// Allow only DBT project relevant paths
							if allowed_example_rel_path(&rel_str) {
								if let Ok(bytes) = fs::read(&p2) {
									files.push((p2.clone(), bytes));
								}
							}
						}
					}
				}
			}
			out.push((project, files));
		}
	}
	out
}

pub async fn sync_from_repo_to_s3_and_embeddings(
	storage: Arc<dyn StorageAdapter>,
	keyspace: Arc<dyn Keyspace>,
	scope: RequestScope,
	llm: Arc<dyn crate::llm::LargeLanguageModel>,
) -> Result<(), String> {
	let root = local_examples_root();
	let projects = collect_local_projects(&root);
	if projects.is_empty() {
		info!("dbt_examples: no local projects under {:?}", root);
		return Ok(());
	}
	let mut g = load_global_manifest(storage.clone()).await;
	let mut updated_projects: Vec<ProjectManifest> = Vec::new();
	let store = crate::react::providers::SkipprLanceVectorStore::new(keyspace, scope.clone()).global_dbt_examples_store();
	for (project, files) in projects.iter() {
		let mut pf: Vec<ProjectFile> = Vec::new();
		let mut changed: Vec<(String, Vec<u8>)> = Vec::new();
		for (pathbuf, bytes) in files.iter() {
			let rel = pathbuf.strip_prefix(&root.join(project)).unwrap_or(pathbuf).to_string_lossy().to_string();
			// Filter again defensively
			let rel_ok = {
				let r = rel.replace('\\', "/");
				r.starts_with("models/")
					|| r.starts_with("metrics/")
					|| r.starts_with("macros/")
					|| r.starts_with("snapshots/")
					|| r.starts_with("seeds/")
					|| r.starts_with("analyses/")
					|| r.starts_with("tests/")
					|| r.starts_with("exposures/")
					|| r.starts_with("docs/")
					|| r == "dbt_project.yml"
					|| r == "packages.yml"
			};
			if !rel_ok { continue; }
			let sha = sha256_bytes(bytes);
			let meta = fs::metadata(pathbuf).ok();
			let mtime = meta.and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).map(|d| (chrono::Utc::now() - chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::seconds(0))).timestamp()).unwrap_or(0);
			pf.push(ProjectFile { path: rel.clone(), sha256: sha.clone(), bytes: bytes.len(), mtime });
		}
		// Determine changed vs existing
		let prev = g.projects.iter().find(|p| &p.project == project);
		let mut digest_hasher = Sha256::new();
		for f in pf.iter() {
			digest_hasher.update(&f.sha256.as_bytes());
		}
		let project_digest = hex::encode(digest_hasher.finalize());
		let mut manifest = ProjectManifest { project: project.clone(), files: pf.clone(), project_digest: project_digest.clone() };
		let needs_reembed = match prev {
			Some(p0) => p0.project_digest != project_digest,
			None => true,
		};
		// Upload files to S3 path: dbt-examples/projects/<project>/<path>
		for (pathbuf, bytes) in files.iter() {
			let rel = pathbuf.strip_prefix(&root.join(project)).unwrap_or(pathbuf).to_string_lossy().to_string();
			// Only upload allowed example paths
			let r = rel.replace('\\', "/");
			let allowed = r.starts_with("models/")
				|| r.starts_with("metrics/")
				|| r.starts_with("macros/")
				|| r.starts_with("snapshots/")
				|| r.starts_with("seeds/")
				|| r.starts_with("analyses/")
				|| r.starts_with("tests/")
				|| r.starts_with("exposures/")
				|| r.starts_with("docs/")
				|| r == "dbt_project.yml"
				|| r == "packages.yml";
			if !allowed { continue; }
			let key = format!("dbt-examples/projects/{}/{}", project, r);
			let ct = if rel.ends_with(".sql") { "text/sql" } else if rel.ends_with(".yaml") || rel.ends_with(".yml") { "text/yaml" } else if rel.ends_with(".md") { "text/markdown" } else { "text/plain" };
			let _ = storage.put_bytes(&key, bytes.as_slice(), ct).await;
		}
		if needs_reembed {
			// Build chunks
			let mut chunks: Vec<Chunk> = Vec::new();
			let epoch = chrono::Utc::now().timestamp() as u64;
			for (pathbuf, bytes) in files.iter() {
				let rel = pathbuf.strip_prefix(&root.join(project)).unwrap_or(pathbuf).to_string_lossy().to_string();
				let r = rel.replace('\\', "/");
				let allowed = r.starts_with("models/")
					|| r.starts_with("metrics/")
					|| r.starts_with("macros/")
					|| r.starts_with("snapshots/")
					|| r.starts_with("seeds/")
					|| r.starts_with("analyses/")
					|| r.starts_with("tests/")
					|| r.starts_with("exposures/")
					|| r.starts_with("docs/")
					|| r == "dbt_project.yml"
					|| r == "packages.yml";
				if !allowed { continue; }
				let text = String::from_utf8_lossy(bytes).to_string();
				let parts = chunk_text(&text, 1500);
				for (idx, part) in parts.iter().enumerate() {
					let id = format!("dbt_example:{}:{}:{}", project, r, idx);
					let meta = serde_json::json!({
						"project": project,
						"path": r,
					});
					chunks.push(Chunk {
						id,
						kind: "dbt_example".to_string(),
						namespace: project.clone(),
						field: Some(r.clone()),
						text: part.clone(),
						vector: Vec::new(),
						meta,
						epoch,
					});
				}
			}
			// Embed in batches
			let batch = 64usize;
			let mut i = 0usize;
			while i < chunks.len() {
				let j = (i + batch).min(chunks.len());
				let texts: Vec<String> = chunks[i..j].iter().map(|c| c.text.clone()).collect();
				let vecs = llm.embed(&texts).map_err(|e| e.to_string())?;
				for (k, v) in vecs.into_iter().enumerate() {
					chunks[i + k].vector = v;
				}
				i = j;
			}
			// Upsert
			let mut p = 0usize;
			let total = chunks.len();
			while p < chunks.len() {
				let q = (p + batch).min(chunks.len());
				store.upsert(&chunks[p..q]).await?;
				info!("dbt_examples: upserted {} / {} chunks for project {}", q, total, project);
				p = q;
			}
		}
		updated_projects.push(manifest);
	}
	g.projects = updated_projects;
	save_global_manifest(storage.clone(), &g).await;
	info!("dbt_examples: sync complete (projects={})", g.projects.len());
	Ok(())
}

pub async fn search_examples(
	keyspace: Arc<dyn Keyspace>,
	scope: RequestScope,
	llm: Arc<dyn crate::llm::LargeLanguageModel>,
	query: &str,
	k: usize,
) -> Result<Vec<ScoredChunk>, String> {
	let vecs = llm.embed(&[query.to_string()]).map_err(|e| e.to_string())?;
	let qvec = vecs.get(0).ok_or_else(|| "embed failed".to_string())?;
	let store = crate::react::providers::SkipprLanceVectorStore::new(keyspace, scope).global_dbt_examples_store();
	let res = store.query(qvec, k).await?;
	Ok(res)
}


