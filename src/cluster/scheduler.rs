use std::sync::Arc;
use std::time::Duration;

use skippr_lease::{
    Clock, DurableError, PipelineKey, PipelineLeaseStore, PipelineLifecycle, PipelinePaths,
    PromoteError, Sleeper, SystemClock, TokioSleeper,
};

use crate::buffer::durable::replicate::{QuorumReplicator, ReplicationMode};
use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle, PipelineWalHandle};
use crate::cluster::gossip::{GossipAd, GossipService};
use crate::cluster::identity::ClusterConfig;
use crate::cluster::membership::{MembershipEndpoints, MembershipService};
use crate::cluster::peer::{
    drop_replica, prune_to_at, query_status, ReplicaRegistry, ReplicaServer, ReplicaSession,
    TcpReplicaClient,
};
use crate::cluster::pipeline_view::PipelineConfigView;
use crate::cluster::placement::rank_replicas;
use crate::cluster::promote::{
    assign_reachable_replica, candidates_from_gossip, promote_pipeline, statuses_from_gossip,
    wait_until_replica_ready, PromoteContext,
};
use crate::helpers::configuration::Config;
use crate::metrics::counters as metrics;
use crate::query_flight::QueryFlightServer;

pub async fn run_clustered(app_cfg: Config, config: ClusterConfig) -> Result<(), DurableError> {
    let identity = config.identity();
    crate::query_flight::runtime::start_clustered(config.advertised_ip).await?;
    let registry = ReplicaRegistry::new(identity.clone());
    let replica =
        ReplicaServer::start_with_registry("0.0.0.0:0".parse().unwrap(), registry.clone()).await?;
    let flight = QueryFlightServer::start_with_registry(
        "0.0.0.0:0".parse().unwrap(),
        identity.clone(),
        Some(registry.clone()),
        app_cfg.clone(),
    )
    .await?;
    let gossip = Arc::new(
        GossipService::start_for_cluster(
            "0.0.0.0:0".parse().unwrap(),
            Vec::new(),
            config.cluster_id.clone(),
            config.node_id,
            config.advertised_ip,
            config.gossip_hmac_key.clone(),
        )
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?,
    );
    crate::cluster::gossip::install_gossip(gossip.clone());
    crate::cluster::peer::install_process_registry(registry.clone());
    let advertised_replica = config.advertised_addr(replica.bind_addr());
    let advertised_flight = config.advertised_addr(flight.bind_addr());
    let advertised_gossip = config.advertised_addr(gossip.bind_addr());
    crate::cluster::identity::install_process_query_bind(
        crate::cluster::identity::ProcessQueryBind {
            identity: identity.clone(),
            flight: advertised_flight,
        },
    );
    let membership = MembershipService::start(
        &app_cfg,
        config.clone(),
        MembershipEndpoints {
            replica: advertised_replica,
            flight: advertised_flight,
            gossip: advertised_gossip,
        },
    )
    .await
    .map_err(DurableError::Io)?;
    gossip.add_seeds(membership.cold_gossip_seeds().await).await;
    let endpoints = MembershipEndpoints {
        replica: advertised_replica,
        flight: advertised_flight,
        gossip: advertised_gossip,
    };
    let advertised_worker = crate::query_flight::runtime::advertised_worker();
    let mut ad = GossipAd::for_node(config.node_id, config.host_id.clone(), endpoints, false);
    ad.worker = advertised_worker;
    gossip.publish_ad(ad.clone()).await;
    tracing::info!(
        replica = %advertised_replica,
        flight = %advertised_flight,
        gossip = %advertised_gossip,
        worker = ?advertised_worker,
        cluster = %config.cluster_id,
        "clustered scheduler started"
    );
    metrics::set_cluster_ready(0);
    let keys = crate::cluster::pipeline_registry::scheduled_pipeline_keys(&app_cfg)?;
    for key in &keys {
        let view = PipelineConfigView::for_registry(&app_cfg, key)
            .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
        view.validate_clustered_sink()
            .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
        let paths = PipelinePaths::new(&config.data_root, &view.key)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let guard = skippr_lease::LeaseGuard::replica(
            view.key.clone(),
            skippr_lease::LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let fence = skippr_wal_catalog::CatalogFence {
            owner: config.node_id,
            epoch: skippr_lease::LeaseEpoch::new(1),
        };
        let catalog = crate::pipeline_backend::open_wal_catalog_for_config(&app_cfg, None, fence)
            .map_err(DurableError::Io)?;
        let offsets =
            crate::cluster::backend::open_cluster_offset_store(&app_cfg, &config, &view.key)?;
        let handle = PipelineWalHandle::new(
            view.key.clone(),
            paths.clone(),
            guard.clone(),
            catalog,
            offsets,
        );
        crate::buffer::wal_handle::install_wal_handle(handle.clone());
        crate::buffer::wal_startup::wal_attach_replica(handle.as_ref())?;
        registry
            .insert(ReplicaSession::new(view.key.clone(), paths, guard))
            .await;
    }
    let mut ready_ad = ad;
    ready_ad.ready = registry.advertised_ready().await;
    gossip.publish_ad(ready_ad).await;
    metrics::set_cluster_ready(registry.advertised_ready().await as u64);
    let mut fence_rx = gossip.subscribe_fences();
    let fence_registry = registry.clone();
    tokio::spawn(async move {
        loop {
            match fence_rx.recv().await {
                Ok((key, epoch)) => {
                    if let Some(session) = fence_registry.get(&key).await {
                        GossipService::apply_fence_rumor(&session.guard, epoch).await;
                        metrics::add_cluster_fence(1);
                    }
                    if let Some(handle) = crate::buffer::wal_handle::wal_handle_for(&key) {
                        GossipService::apply_fence_rumor(handle.guard(), epoch).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let scheduler = PipelineScheduler::new();
    let clock = Arc::new(SystemClock::new());
    let sleeper = Arc::new(TokioSleeper::new(clock.clone()));
    let membership = Arc::new(membership);
    crate::cluster::peer::install_rpc_clock(clock.clone(), sleeper.clone());
    membership
        .set_ready(registry.advertised_ready().await)
        .await;
    membership.tick().await;
    gossip.add_seeds(membership.cold_gossip_seeds().await).await;
    let advertise_membership = membership.clone();
    let advertise_gossip = gossip.clone();
    let advertise_registry = registry.clone();
    let advertise_sleeper = sleeper.clone();
    let advertise_clock = clock.clone();
    let mut advertise_base =
        GossipAd::for_node(config.node_id, config.host_id.clone(), endpoints, false);
    advertise_base.worker = advertised_worker;
    tokio::spawn(async move {
        let mut gossip_heartbeat = 0u64;
        loop {
            advertise_gossip
                .add_seeds(advertise_membership.cold_gossip_seeds().await)
                .await;
            let ready = advertise_registry.advertised_ready().await;
            advertise_membership.set_ready(ready).await;
            gossip_heartbeat = gossip_heartbeat.saturating_add(1);
            let mut ad = advertise_base.clone();
            ad.ready = ready;
            ad.heartbeat = gossip_heartbeat;
            ad.disk_pressure =
                crate::cluster::disk::disk_under_pressure(&advertise_membership.config().data_root);
            ad.wal_heads =
                crate::cluster::wal_head::collect_local_wal_heads(Some(&advertise_registry)).await;
            advertise_gossip.publish_ad(ad).await;
            metrics::set_cluster_ready(ready as u64);
            advertise_sleeper
                .sleep_until(
                    advertise_clock
                        .monotonic_now()
                        .saturating_add(skippr_lease::LEASE_RENEW_PERIOD),
                )
                .await;
        }
    });
    let membership_task = tokio::spawn(crate::cluster::membership::renew_loop(
        membership.clone(),
        sleeper.clone(),
        clock.clone(),
    ));
    let leases: Arc<dyn PipelineLeaseStore> =
        crate::cluster::backend::open_lease_store(&app_cfg, config.table.clone())
            .await
            .map_err(|err| DurableError::Io(err))?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("clustered scheduler received SIGINT/SIGTERM");
            scheduler.stop_acquiring();
        }
        _ = run_primary_loop(
            app_cfg.clone(),
            config.clone(),
            advertised_replica,
            gossip.clone(),
            membership.clone(),
            clock.clone(),
            sleeper.clone(),
            scheduler.stop.clone(),
            leases.clone(),
        ) => {}
        _ = scheduler.wait_until_stopped() => {}
    }
    membership_task.abort();
    for handle in crate::buffer::wal_handle::all_wal_handles() {
        handle.guard().begin_drain();
    }
    let catalog_ok = crate::catalog_coordinator::drain_catalog_outboxes(Duration::from_secs(30))
        .await
        .is_ok();
    let quiescent = match crate::helpers::offsets::Offsets::init(&app_cfg) {
        Ok(offsets) => {
            catalog_ok
                && crate::buffer::ingest_buffer::Buffers::drain_and_stop_compactor(
                    &app_cfg,
                    Arc::new(offsets),
                )
                .await
        }
        Err(_) => false,
    };
    let release = crate::buffer::wal_handle::all_wal_handles()
        .into_iter()
        .find_map(|handle| {
            handle
                .guard()
                .leased_session()
                .map(|session| (handle.key().clone(), session))
        });
    let lease_release = if quiescent {
        release
            .as_ref()
            .map(|(key, session)| crate::cluster::lifecycle::LeaseRelease {
                store: leases.as_ref(),
                key,
                session,
            })
    } else {
        None
    };
    crate::cluster::lifecycle::shutdown_cluster(&replica, &flight, gossip.as_ref(), lease_release)
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    Ok(())
}

async fn run_primary_loop(
    app_cfg: Config,
    config: ClusterConfig,
    local_replica: std::net::SocketAddr,
    gossip: Arc<GossipService>,
    membership: Arc<MembershipService>,
    clock: Arc<dyn skippr_lease::Clock>,
    sleeper: Arc<dyn skippr_lease::Sleeper>,
    stop: tokio::sync::watch::Sender<PipelineLifecycle>,
    leases: Arc<dyn PipelineLeaseStore>,
) {
    let mut suspect_rx = gossip.subscribe_suspects();
    loop {
        if matches!(
            *stop.subscribe().borrow(),
            PipelineLifecycle::Draining | PipelineLifecycle::Fenced
        ) {
            break;
        }
        let keys = match crate::cluster::pipeline_registry::scheduled_pipeline_keys(&app_cfg) {
            Ok(keys) => keys,
            Err(err) => {
                tracing::error!(error = %err, "pipeline registry unavailable; fail closed");
                tokio::select! {
                    _ = sleeper.sleep_until(clock.monotonic_now().saturating_add(Duration::from_secs(1))) => {}
                    rumor = suspect_rx.recv() => { let _ = rumor; }
                }
                continue;
            }
        };
        let mut ran = false;
        for key in keys {
            let name = key.pipeline().to_string();
            if matches!(
                *stop.subscribe().borrow(),
                PipelineLifecycle::Draining | PipelineLifecycle::Fenced
            ) {
                break;
            }
            match try_promote_and_ingest(
                &app_cfg,
                &config,
                local_replica,
                gossip.clone(),
                membership.clone(),
                clock.clone(),
                sleeper.clone(),
                leases.clone(),
                &key,
            )
            .await
            {
                Ok(PrimaryRun::HeldUntilFence) => ran = true,
                Ok(PrimaryRun::ReleasedAfterFinite) => {}
                Err(err) => tracing::warn!(pipeline = %name, error = %err, "promote skipped"),
            }
            if ran {
                break;
            }
        }
        tokio::select! {
            _ = sleeper.sleep_until(clock.monotonic_now().saturating_add(Duration::from_secs(1))) => {}
            rumor = suspect_rx.recv() => {
                match rumor {
                    Ok(rumor) => {
                        metrics::add_cluster_suspect(1);
                        tracing::info!(
                            pipeline = rumor.pipeline.as_deref().unwrap_or("*"),
                            "gossip suspect; retrying observe/steal"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

enum PrimaryRun {
    HeldUntilFence,
    ReleasedAfterFinite,
}

fn holds_lease_after_source_complete(source_plugin: &str) -> bool {
    source_plugin.eq_ignore_ascii_case("File")
}

async fn try_promote_and_ingest(
    app_cfg: &Config,
    config: &ClusterConfig,
    local_replica: std::net::SocketAddr,
    gossip: Arc<GossipService>,
    membership: Arc<MembershipService>,
    clock: Arc<dyn skippr_lease::Clock>,
    sleeper: Arc<dyn skippr_lease::Sleeper>,
    leases: Arc<dyn PipelineLeaseStore>,
    key: &skippr_lease::PipelineKey,
) -> Result<PrimaryRun, DurableError> {
    let view = PipelineConfigView::for_registry(app_cfg, key)
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
    let name = key.pipeline();
    let paths = PipelinePaths::new(&config.data_root, &view.key)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    gossip.add_seeds(membership.cold_gossip_seeds().await).await;
    let identity = config.identity();
    let statuses = statuses_from_gossip(&gossip, &view.key, &identity).await;
    let candidates = replica_candidates_for(&gossip, &membership).await;
    let ctx = PromoteContext {
        paths: paths.clone(),
        identity: identity.clone(),
        local_replica,
        gossip: gossip.clone(),
    };
    let outcome = promote_pipeline(
        leases.clone(),
        clock.clone(),
        sleeper.clone(),
        view.key.clone(),
        config.node_id,
        statuses,
        candidates,
        &config.host_id,
        ctx,
    )
    .await
    .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
    let renew_guard = outcome.guard.clone();
    let renew_key = view.key.clone();
    let renew_clock = clock.clone();
    let renew_sleeper = sleeper.clone();
    let renew_leases = leases.clone();
    let renew_task = tokio::spawn(async move {
        skippr_lease::renew_until_lost(
            renew_leases.as_ref(),
            renew_clock.as_ref(),
            renew_sleeper.as_ref(),
            &renew_key,
            renew_guard.as_ref(),
        )
        .await;
        metrics::add_cluster_lease_lost(1);
    });
    let prepared = async {
        let replica_client = TcpReplicaClient::new(outcome.replica_endpoint, identity.clone());
        let replicator = ReplicationMode::Synchronous(QuorumReplicator::new(
            replica_client.clone(),
            outcome.replica_endpoint,
        ));
        let session = outcome.guard.leased_session().ok_or(DurableError::Fenced)?;
        let fence = skippr_wal_catalog::CatalogFence {
            owner: session.owner,
            epoch: session.epoch,
        };
        let catalog = crate::pipeline_backend::open_wal_catalog_for_config(app_cfg, None, fence)
            .map_err(|err| DurableError::Io(err))?;
        let offsets =
            crate::cluster::backend::open_cluster_offset_store(app_cfg, config, &view.key)?;
        let handle = PipelineWalHandle::with_replication(
            view.key.clone(),
            paths.clone(),
            outcome.guard.clone(),
            catalog,
            offsets,
            replicator,
        );
        install_wal_handle(handle.clone());
        crate::cluster::schema::load_and_install_pipeline_schema(
            app_cfg,
            &view.key,
            view.flatten_events,
            session.initialized,
        )
        .await
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
        crate::buffer::wal_startup::wal_startup(&handle, app_cfg, true).await?;
        let live = handle.catalog_segment_ids()?;
        let prune_epoch = outcome
            .guard
            .leased_session()
            .ok_or(DurableError::Fenced)?
            .epoch
            .get();
        prune_to_at(
            outcome.replica_endpoint,
            &view.key,
            live,
            prune_epoch,
            &identity,
        )
        .await?;
        let mut session = outcome.guard.leased_session().ok_or(DurableError::Fenced)?;
        session.initialized = true;
        if outcome.needs_initialize {
            leases
                .mark_initialized(&view.key, &session)
                .await
                .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
        }
        outcome
            .guard
            .activate(session.clone())
            .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
        Ok::<_, DurableError>((handle, replica_client, session))
    }
    .await;
    let (_handle, replica_client, session) = match prepared {
        Ok(parts) => parts,
        Err(err) => {
            outcome.guard.fence();
            renew_task.abort();
            if let Err(release_err) = leases
                .release_after_drain(&view.key, &outcome.session)
                .await
            {
                tracing::error!(
                    error = %release_err,
                    "unactivated primary could not release lease"
                );
            }
            return Err(err);
        }
    };
    let _ = ();
    tracing::info!(
        pipeline = %name,
        pipeline.type = app_cfg.bind_pipeline(name).pipeline_type_log_label(),
        "clustered primary ingest started"
    );
    metrics::set_cluster_primary(1);
    metrics::set_cluster_ready(1);
    let replace_client = replica_client.clone();
    let replace_gossip = gossip.clone();
    let replace_membership = membership.clone();
    let replace_key = view.key.clone();
    let replace_identity = identity.clone();
    let replace_host = config.host_id.clone();
    let replace_node = config.node_id;
    let replace_clock = clock.clone();
    let replace_sleeper = sleeper.clone();
    let replace_epoch = session.epoch;
    let replace_primary = local_replica;
    let replace_task = tokio::spawn(async move {
        loop {
            replace_sleeper
                .sleep_until(
                    replace_clock
                        .monotonic_now()
                        .saturating_add(skippr_lease::LEASE_RENEW_PERIOD),
                )
                .await;
            let current = replace_client.endpoint();
            if query_status(current, &replace_key, &replace_identity)
                .await
                .is_ok()
            {
                continue;
            }
            replace_gossip
                .add_seeds(replace_membership.cold_gossip_seeds().await)
                .await;
            let ranked = rank_replicas(
                &replace_key,
                replace_node,
                &replace_host,
                replica_candidates_for(&replace_gossip, &replace_membership).await,
            );
            let Ok(endpoint) = assign_reachable_replica(
                &ranked,
                &replace_key,
                replace_epoch,
                replace_primary,
                &replace_identity,
                Some(current),
            )
            .await
            else {
                continue;
            };
            let Ok(status) = query_status(endpoint, &replace_key, &replace_identity).await else {
                continue;
            };
            if let Some(handle) = crate::buffer::wal_handle::wal_handle_for(&replace_key) {
                let local = handle.catalog_seq_seen().unwrap_or(0);
                metrics::set_cluster_replica_lag(local.saturating_sub(status.catalog_seq_seen));
            }
            let Ok(_) = wait_until_replica_ready(
                endpoint,
                &replace_key,
                &replace_identity,
                replace_clock.as_ref(),
                replace_sleeper.as_ref(),
                status,
            )
            .await
            else {
                continue;
            };
            replace_client.retarget(endpoint);
            let _ = drop_replica(current, &replace_key, &replace_identity).await;
        }
    });
    loop {
        if matches!(
            outcome.guard.lifecycle(),
            skippr_lease::PipelineLifecycle::Fenced | skippr_lease::PipelineLifecycle::Draining
        ) {
            break;
        }
        let ingest = crate::engine::run_sync_pipeline(app_cfg, name, "text", false);
        match outcome.guard.run_until_fenced(ingest).await {
            Ok(Ok(()))
                if !matches!(
                    outcome.guard.lifecycle(),
                    skippr_lease::PipelineLifecycle::Fenced
                        | skippr_lease::PipelineLifecycle::Draining
                ) =>
            {
                if !holds_lease_after_source_complete(&view.source_plugin) {
                    tracing::info!(
                        pipeline = %name,
                        source = %view.source_plugin,
                        "clustered finite source complete; releasing lease"
                    );
                    outcome.guard.begin_drain();
                    leases
                        .release_after_drain(&view.key, &session)
                        .await
                        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
                    renew_task.abort();
                    replace_task.abort();
                    metrics::set_cluster_primary(0);
                    metrics::set_cluster_ready(0);
                    remove_wal_handle(&view.key);
                    return Ok(PrimaryRun::ReleasedAfterFinite);
                }
                tracing::info!(
                    pipeline = %name,
                    "clustered source scan complete; remaining primary until fenced"
                );
                let mut seen = file_source_mtime(app_cfg);
                loop {
                    if matches!(
                        outcome.guard.lifecycle(),
                        skippr_lease::PipelineLifecycle::Fenced
                            | skippr_lease::PipelineLifecycle::Draining
                    ) {
                        break;
                    }
                    sleeper
                        .sleep_until(
                            clock
                                .monotonic_now()
                                .saturating_add(skippr_lease::LEASE_RENEW_PERIOD),
                        )
                        .await;
                    let now = file_source_mtime(app_cfg);
                    if now > seen {
                        seen = now;
                        break;
                    }
                }
            }
            Ok(Err(err))
                if !matches!(
                    outcome.guard.lifecycle(),
                    skippr_lease::PipelineLifecycle::Fenced
                        | skippr_lease::PipelineLifecycle::Draining
                ) =>
            {
                tracing::warn!(
                    pipeline = %name,
                    error = %err,
                    "clustered ingest failed; retrying while still primary"
                );
                sleeper
                    .sleep_until(
                        clock
                            .monotonic_now()
                            .saturating_add(skippr_lease::LEASE_RENEW_PERIOD),
                    )
                    .await;
            }
            _ => break,
        }
    }
    renew_task.abort();
    replace_task.abort();
    metrics::set_cluster_primary(0);
    metrics::set_cluster_ready(0);
    remove_wal_handle(&view.key);
    Ok(PrimaryRun::HeldUntilFence)
}

fn file_source_mtime(config: &Config) -> Option<std::time::SystemTime> {
    let sources = config.data_sources.as_ref()?;
    let mut newest = None::<std::time::SystemTime>;
    for entry in sources.values() {
        if !entry.plugin_name.eq_ignore_ascii_case("File") {
            continue;
        }
        let Some(path) = entry.config.get("path").and_then(|value| value.as_str()) else {
            continue;
        };
        let path = std::path::Path::new(path);
        let mut bump = |mtime: std::time::SystemTime| {
            newest = Some(newest.map_or(mtime, |prev| prev.max(mtime)));
        };
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(mtime) = meta.modified() {
                bump(mtime);
            }
        }
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Ok(mtime) = entry.metadata().and_then(|meta| meta.modified()) {
                    bump(mtime);
                }
            }
        }
    }
    newest
}

async fn replica_candidates_for(
    gossip: &GossipService,
    membership: &MembershipService,
) -> Vec<crate::cluster::placement::ReplicaCandidate> {
    let ads = gossip.known_ads().await;
    let mut candidates = candidates_from_gossip(&ads);
    let self_id = membership.config().node_id;
    if candidates
        .iter()
        .any(|candidate| candidate.node_id != self_id)
    {
        return candidates;
    }
    for extra in membership.replica_candidates().await {
        if extra.node_id != self_id
            && candidates
                .iter()
                .all(|candidate| candidate.node_id != extra.node_id)
        {
            candidates.push(extra);
        }
    }
    candidates
}

pub async fn clustered_query_collect(
    app_cfg: &Config,
    sql: &str,
) -> Result<Vec<arrow::array::RecordBatch>, DurableError> {
    let storage = app_cfg.get_wal_storage();
    let config = crate::cluster::validation::validate_clustered_mode(
        app_cfg,
        storage,
        crate::cluster::validation::CliModeKind::Query,
    )
    .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?
    .ok_or_else(|| {
        DurableError::ProtocolMismatch("clustered query requires ClusterConfig".into())
    })?;
    let store = crate::cluster::backend::open_membership_store(app_cfg, config.table.clone())
        .await
        .map_err(DurableError::Io)?;
    let members = store
        .query_cluster(&config.cluster_id)
        .await
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let mut ready: Vec<_> = members
        .into_iter()
        .filter(|record| record.ad.ready)
        .collect();
    ready.sort_by_key(|record| record.ad.node_id.to_string());
    let mut last_err: Option<String>;
    let mut pin_attempt = 0u32;
    loop {
        let mut pin_lost = false;
        last_err = None;
        for record in &ready {
            let endpoint = record.ad.flight_addr;
            match crate::query_flight::client::fetch_flight_sql(endpoint, sql).await {
                Ok(batches) => {
                    let rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
                    tracing::info!(
                        flight = %endpoint,
                        rows,
                        "clustered query Flight SQL"
                    );
                    return Ok(batches);
                }
                Err(err) => {
                    let text = err.to_string();
                    if crate::sqlrt::wal_table::is_wal_pin_lost_text(&text) {
                        pin_lost = true;
                        last_err = Some(text);
                        break;
                    }
                    tracing::info!(
                        flight = %endpoint,
                        error = %err,
                        "clustered query Flight SQL contact failed; trying next ready node"
                    );
                    last_err = Some(text);
                }
            }
        }
        if pin_lost {
            pin_attempt += 1;
            if pin_attempt < crate::sqlrt::wal_table::QUERY_PIN_RETRY_CEILING {
                continue;
            }
            return Err(DurableError::Io(last_err.unwrap_or_else(|| {
                "WAL slice was reclaimed after the query pinned it; retry the query".into()
            })));
        }
        break;
    }
    Err(DurableError::Io(format!(
        "no ready Flight SQL node: {}",
        last_err.unwrap_or_else(|| "no ready Flight SQL node".into())
    )))
}

pub struct PipelineScheduler {
    stop: tokio::sync::watch::Sender<PipelineLifecycle>,
    rx: tokio::sync::watch::Receiver<PipelineLifecycle>,
}

impl PipelineScheduler {
    pub fn new() -> Self {
        let (stop, rx) = tokio::sync::watch::channel(PipelineLifecycle::Idle);
        Self { stop, rx }
    }

    pub fn stop_acquiring(&self) {
        let _ = self.stop.send_replace(PipelineLifecycle::Draining);
    }

    pub async fn wait_until_stopped(&self) {
        let mut rx = self.rx.clone();
        while !matches!(
            *rx.borrow(),
            PipelineLifecycle::Draining | PipelineLifecycle::Fenced
        ) {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_stop_is_idempotent() {
        let scheduler = PipelineScheduler::new();
        scheduler.stop_acquiring();
        scheduler.stop_acquiring();
    }

    #[tokio::test]
    async fn wait_until_stopped_returns_after_drain() {
        let scheduler = PipelineScheduler::new();
        scheduler.stop_acquiring();
        scheduler.wait_until_stopped().await;
    }

    #[test]
    fn file_sources_hold_lease_after_scan_other_plugins_release() {
        assert!(holds_lease_after_source_complete("File"));
        assert!(holds_lease_after_source_complete("file"));
        assert!(!holds_lease_after_source_complete("S3"));
        assert!(!holds_lease_after_source_complete("Kafka"));
    }

    #[test]
    fn primary_schema_loads_after_writer_handle() {
        let src = include_str!("scheduler.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let install = prod
            .find("install_wal_handle(handle.clone())")
            .expect("install_wal_handle");
        let schema = prod
            .find("load_and_install_pipeline_schema")
            .expect("schema after catalog open");
        assert!(
            install < schema,
            "schema must see Skippr Store via the installed writer handle"
        );
        let promote = include_str!("promote.rs");
        let promote_prod = promote.split("#[cfg(test)]").next().unwrap();
        assert!(
            !promote_prod.contains("load_and_install_pipeline_schema"),
            "promote must not consult WAL namespaces before the catalog is open"
        );
    }

    #[test]
    fn payload_missing_is_logged_and_retried() {
        let src = include_str!("scheduler.rs");
        assert!(src.contains("promote skipped"));
        assert!(!src.contains(concat!("Unproven", "Prepared")));
        assert!(src.contains("unactivated primary could not release lease"));
        assert!(src.contains("prune_to_at"));
        assert!(src.contains("wal_attach_replica"));
        assert!(!src.contains(concat!("resolve_legacy", "_root")));
        assert!(!src.contains(concat!("legacy", "_root")));
    }

    #[test]
    fn clustered_query_fails_closed_without_flight() {
        let src = include_str!("scheduler.rs");
        let query = src
            .split("pub async fn clustered_query_collect")
            .nth(1)
            .unwrap()
            .split("pub struct PipelineScheduler")
            .next()
            .unwrap();
        assert!(query.contains("fetch_flight_sql"));
        assert!(query.contains("no ready Flight SQL node"));
        assert!(query.contains("QUERY_PIN_RETRY_CEILING"));
        assert!(query.contains("is_wal_pin_lost_text"));
        assert!(!query.contains("Iceberg-only"));
        assert!(!query.contains("run_lake_only_query"));
        assert!(!query.contains("query_status"));
        assert!(!query.contains("query_schedulers"));
        assert!(!query.contains("select_highest_hash_consistent"));
        assert!(!query.contains("skipped unreachable replica"));
        assert!(!query.contains("execute_clustered_select"));
        assert!(
            !query.contains("print_batches_plain"),
            "clustered_query_collect returns batches; CLI Session.query prints"
        );
        assert!(
            !query.contains("print_query_plain_json"),
            "clustered_query_collect returns batches; CLI Session.query prints"
        );
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("run_clustered_from_config"));
        assert!(!prod.contains("fn run_clustered_query"));
    }
}
