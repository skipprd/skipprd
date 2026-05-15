//! `skippr vector ingest-docs` — declarative file walk, chunk, embed, Lance upsert (tenant bucket).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use react_core::provider_traits::upsert_typed_documents;
use react_core::scope::RequestScope;
use react_suite_data_engineer::vector_docs::ManualVectorDocument;
use walkdir::WalkDir;

use crate::api_client;
use crate::public_config::SkipprProjectConfig;
use crate::react_host::vector::LanceVectorStore;
use crate::translate;

const DEFAULT_CHUNK_CHARS: usize = 1200;
const DEFAULT_CHUNK_OVERLAP: usize = 120;
const EMBED_BATCH: usize = 32;

fn posix_rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn build_glob_set(patterns: &[String], root_label: &str) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let t = p.trim();
        if t.is_empty() {
            continue;
        }
        let g = Glob::new(t).map_err(|e| format!("invalid glob in {root_label}: {e}"))?;
        b.add(g);
    }
    b.build().map_err(|e| format!("invalid glob set ({root_label}): {e}"))
}

fn chunk_text(text: &str, size: usize, overlap: usize) -> Vec<String> {
    if size == 0 {
        return vec![text.to_string()];
    }
    let step = size.saturating_sub(overlap).max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = (i + size).min(chars.len());
        let piece: String = chars[i..end].iter().collect();
        if !piece.trim().is_empty() {
            out.push(piece);
        }
        if end >= chars.len() {
            break;
        }
        i += step;
    }
    out
}

pub struct VectorIngestDocsArgs {
    pub config: Option<PathBuf>,
    /// `pipelines.<name>` that defines `vector_source` (default pipeline name: `vector_ingest`).
    pub pipeline: String,
    pub vector_source: Option<String>,
    pub src_path: Option<PathBuf>,
    pub chunk_chars: Option<usize>,
    pub chunk_overlap: Option<usize>,
    pub include_glob: Vec<String>,
    pub exclude_glob: Vec<String>,
    pub dry_run: bool,
    pub output: String,
}

pub async fn run_vector_ingest_docs(args: VectorIngestDocsArgs) {
    let explicit = &args.config;
    let cfg_path = crate::config_path(explicit);
    crate::load_dotenv_for_skippr_config_yaml_path(&cfg_path);

    let engine_yaml = match crate::load_engine_config(explicit) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let public_cfg = match SkipprProjectConfig::load_resolved_from(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    if public_cfg.vector_sources.is_empty() {
        eprintln!("[skippr] ERROR: skippr.yml must define vector_sources for ingest-docs.");
        std::process::exit(1);
    }

    let pipeline_spec = match public_cfg.vector_ingest_pipeline_spec(&args.pipeline) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let source_key = args
        .vector_source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| pipeline_spec.vector_source.clone());

    let Some(entry) = public_cfg.vector_sources.get(&source_key).cloned() else {
        eprintln!(
            "[skippr] ERROR: unknown vector_sources key '{source_key}' (from pipelines.{}.vector_source or --vector-source). Known: {:?}",
            args.pipeline.trim(),
            public_cfg.vector_sources.keys().collect::<Vec<_>>()
        );
        std::process::exit(1);
    };

    let workspace = crate::yaml_string_at(&engine_yaml, &["skippr", "workspace"])
        .unwrap_or("default")
        .to_string();

    let cfg_dir = cfg_path.parent().unwrap_or(Path::new("."));
    let declared_root = cfg_dir.join(entry.root.trim());
    let scan_root = args
        .src_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| declared_root.clone());
    let scan_root = scan_root.canonicalize().unwrap_or(scan_root);

    if !scan_root.is_dir() {
        eprintln!(
            "[skippr] ERROR: ingest scan root is not a directory: {}",
            scan_root.display()
        );
        std::process::exit(1);
    }

    let chunk_chars = args
        .chunk_chars
        .or(pipeline_spec.chunk_chars)
        .or(entry.chunk_chars)
        .unwrap_or(DEFAULT_CHUNK_CHARS);
    let chunk_overlap = args
        .chunk_overlap
        .or(pipeline_spec.chunk_overlap)
        .or(entry.chunk_overlap)
        .unwrap_or(DEFAULT_CHUNK_OVERLAP);

    let mut include = entry.include.clone();
    include.extend(
        args.include_glob
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    );
    if include.is_empty() {
        eprintln!(
            "[skippr] ERROR: vector_sources.{source_key}.include must list at least one glob (declarative discovery)."
        );
        std::process::exit(1);
    }

    let mut exclude = entry.exclude.clone();
    exclude.extend(
        args.exclude_glob
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    );

    let include_set = match build_glob_set(&include, "include") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    let exclude_set = if exclude.is_empty() {
        GlobSetBuilder::new().build().unwrap_or_else(|e| {
            eprintln!("[skippr] ERROR: internal GlobSet: {e}");
            std::process::exit(1);
        })
    } else {
        match build_glob_set(&exclude, "exclude") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[skippr] ERROR: {e}");
                std::process::exit(1);
            }
        }
    };

    let ext_filter: Option<Vec<String>> = entry.extensions.clone().map(|v| {
        v.into_iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    });

    let mut files: Vec<PathBuf> = Vec::new();
    for w in WalkDir::new(&scan_root).into_iter().filter_map(Result::ok) {
        if !w.file_type().is_file() {
            continue;
        }
        let path = w.path().to_path_buf();
        let rel = posix_rel(&path, &scan_root);
        if !include_set.is_match(rel.as_str()) {
            continue;
        }
        if exclude_set.is_match(rel.as_str()) {
            continue;
        }
        if let Some(ref exts) = ext_filter {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !exts.iter().any(|allowed| allowed == &ext) {
                continue;
            }
        }
        files.push(path);
    }
    files.sort();

    let mut work_items: Vec<(PathBuf, String, usize)> = Vec::new();
    for path in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "[skippr] WARNING: skip {}: {}",
                    path.display(),
                    e
                );
                continue;
            }
        };
        let chunks = chunk_text(&text, chunk_chars, chunk_overlap);
        for (ci, chunk) in chunks.into_iter().enumerate() {
            work_items.push((path.clone(), chunk, ci));
        }
    }

    if crate::is_json_output(&args.output) {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "dry_run": args.dry_run,
                "pipeline": args.pipeline.trim(),
                "vector_source": source_key,
                "files": files.len(),
                "chunks": work_items.len(),
                "scan_root": scan_root.to_string_lossy(),
            })
        );
    } else {
        eprintln!(
            "[skippr] vector ingest-docs: pipeline={} source={source_key} files={} chunks={} root={}",
            args.pipeline.trim(),
            files.len(),
            work_items.len(),
            scan_root.display()
        );
    }

    if args.dry_run {
        return;
    }

    if work_items.is_empty() {
        eprintln!("[skippr] ERROR: nothing to embed (no matching file contents).");
        std::process::exit(1);
    }

    let authenticated_with_api_key = std::env::var("SKIPPR_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let creds = if let Ok(api_key) = std::env::var("SKIPPR_API_KEY") {
        if api_key.trim().is_empty() {
            eprintln!("[skippr] ERROR: SKIPPR_API_KEY is set but empty.");
            std::process::exit(1);
        }
        let base_url = crate::auth::auth_base_url();
        let client = api_client::ApiClient::new(&base_url);
        match client.exchange_api_key(api_key.trim()).await {
            Ok(tokens) => {
                eprintln!("[skippr] authenticated via API key");
                tokens
            }
            Err(e) => {
                eprintln!("[skippr] ERROR: API key authentication failed: {}", e);
                std::process::exit(1);
            }
        }
    } else if let Some(creds) = crate::auth::load_credentials() {
        eprintln!("[skippr] authenticated via stored credentials");
        crate::refresh_user_credentials_or_exit(
            &api_client::ApiClient::new(&crate::auth::auth_base_url()),
            creds,
        )
        .await
    } else {
        eprintln!("[skippr] ERROR: Authentication required.");
        eprintln!("[skippr]   Run 'skippr user login' or set SKIPPR_API_KEY.");
        std::process::exit(1);
    };

    let base_url = crate::auth::auth_base_url();
    let tokens = crate::create_token_provider(&creds);
    let client = api_client::ApiClient::authenticated(&base_url, std::sync::Arc::clone(&tokens));

    if let Err(e) = crate::ensure_eula_accepted(&client, !authenticated_with_api_key).await {
        eprintln!("[skippr] ERROR: {}", e);
        std::process::exit(1);
    }

    let initial_balance = match client.get_account().await {
        Ok(account) => {
            let bal = account.balance.balance;
            if bal <= 0.0 {
                eprintln!("[skippr] ERROR: Balance is $0.00. Add funds to continue.");
                std::process::exit(1);
            }
            bal
        }
        Err(e) => {
            eprintln!("[skippr] ERROR: Could not verify account balance ({}).", e);
            std::process::exit(1);
        }
    };

    let srv_creds = match client.get_credentials().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: Failed to fetch server credentials: {}", e);
            std::process::exit(1);
        }
    };

    let mut react_file =
        translate::react_config_file_for_vector_doc_ingest(&public_cfg.project, &workspace);
    if let Err(e) = translate::apply_authenticated_overlay(
        &mut react_file,
        &srv_creds,
        std::sync::Arc::clone(&tokens),
        initial_balance,
    ) {
        eprintln!("[skippr] ERROR: {e}");
        std::process::exit(1);
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    react_suite_data_engineer::metering::set_metering_run_id(&run_id);

    let mut resolved = match crate::react_host::resolve_config(
        react_file,
        react::config::ServeOverrides::default(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };
    crate::attach_s3_credentials_provider(&mut resolved, client.clone());

    let mut sctx = match react::bootstrap::build_base_suite_ctx(&resolved).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skippr] ERROR: {e}");
            std::process::exit(1);
        }
    };

    let (lance_uri_prefix, lance_storage_opts) =
        match crate::react_host::lance_storage_for_resolved(&resolved) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[skippr] ERROR: {e}");
                std::process::exit(1);
            }
        };
    let lance = LanceVectorStore::new(lance_uri_prefix).with_storage_options(lance_storage_opts);
    sctx.set_vector(Some(Arc::new(lance)));

    let scope = RequestScope::parse(
        resolved.scope.tenant.as_str(),
        resolved.scope.workspace.as_str(),
        resolved.scope.project_id.as_str(),
    )
    .unwrap_or_else(|e| {
        eprintln!("[skippr] ERROR: invalid scope: {e}");
        std::process::exit(1);
    });

    let epoch = chrono::Utc::now().timestamp() as u64;
    let mut total = 0usize;
    for batch in work_items.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, t, _)| t.clone()).collect();
        let embeddings = match sctx.llm_embed(&texts) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[skippr] ERROR: embed failed: {e}");
                std::process::exit(1);
            }
        };
        let mut docs: Vec<ManualVectorDocument> = Vec::with_capacity(batch.len());
        for (i, (path, text, chunk_idx)) in batch.iter().enumerate() {
            let rel = posix_rel(path, &scan_root);
            let id = format!("docs:{source_key}:{rel}:{chunk_idx}");
            let vec = embeddings.get(i).cloned().unwrap_or_default();
            let meta = serde_json::json!({
                "path": rel,
                "vector_source": source_key,
                "chunk_index": chunk_idx,
            });
            docs.push(ManualVectorDocument::new(
                id,
                text.clone(),
                vec,
                epoch,
                react_suite_data_engineer::vector_docs::ManualVectorMetadata {
                    kind: "docs".into(),
                    dataset_id: Some(source_key.clone()),
                    field: None,
                    extra: meta,
                },
            ));
        }
        let store = match sctx.vector().as_ref() {
            Some(v) => v.clone(),
            None => {
                eprintln!("[skippr] ERROR: vector store not configured");
                std::process::exit(1);
            }
        };
        if let Err(e) = upsert_typed_documents(store.as_ref(), &scope, &docs).await {
            eprintln!("[skippr] ERROR: vector upsert failed: {e}");
            std::process::exit(1);
        }
        total += docs.len();
    }

    if crate::is_json_output(&args.output) {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "ingested_chunks": total,
                "pipeline": args.pipeline.trim(),
                "vector_source": source_key,
                "lancedb_uri": format!(
                    "s3://{}/{}/{}/{}/lancedb",
                    srv_creds.bucket,
                    resolved.scope.tenant,
                    resolved.scope.workspace,
                    resolved.scope.project_id
                ),
            })
        );
    } else {
        eprintln!(
            "[skippr] ingest-docs complete: pipeline={} {} chunks → s3://{}/{}/{}/{}/lancedb",
            args.pipeline.trim(),
            total,
            srv_creds.bucket,
            resolved.scope.tenant,
            resolved.scope.workspace,
            resolved.scope.project_id
        );
    }
}
