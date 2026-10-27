#!/usr/bin/env python3
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parent / "run.py"
SPEC = importlib.util.spec_from_file_location("hla_e2e_run", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
hla_e2e = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = hla_e2e
SPEC.loader.exec_module(hla_e2e)


class HlaE2eHarnessTests(unittest.TestCase):
    def test_scenarios_cover_cluster_failure_and_query_axes(self) -> None:
        self.assertEqual(
            hla_e2e.SCENARIOS,
            [
                "bootstrap_three_nodes",
                "ingest_and_query_union",
                "distributed_cluster_query",
                "dynamo_closed_epoch_asserts",
                "query_from_every_ready_replica",
                "late_node_wal_catchup",
                "primary_kill_failover",
                "sigkill_during_ingest_no_duplicate_rows",
                "after_catalog_commit_no_dupes",
                "schema_evolution",
                "after_replica_payload_ack_no_dupes",
                "after_offsets_published_no_dupes",
                "after_replica_payload_ack",
                "after_compaction_sink",
                "query_during_compaction_sent",
                "snapshot_catchup_fourth_node",
                "sigstop_old_primary",
                "two_node_quorum_stall",
                "sigterm_drain",
                "enospc_before_catalog_commit",
                "restart_generation",
                "donor_kill_mid_catchup",
                "corrupt_replica_purge",
                "before_compaction_sink_replay",
                "query_each_replica_socket",
                "query_retry_lagging",
                "failed_drain_holds_lease",
                "two_pipelines",
                "mixed_protocol_handshake",
                "rolling_protocol_replacement",
                "cold_start_membership",
                "cross_cluster_handshake",
                "same_host_exclusion",
                "clustered_rejects_at_least_once",
            ],
        )

    def test_skippr_yml_uses_file_warehouse_and_skippr_catalog(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "skippr.yml"
            events = Path(tmp) / "events"
            warehouse = Path(tmp) / "warehouse"
            hla_e2e.write_skippr_yml(config, events, warehouse)
            text = config.read_text(encoding="utf-8")
            self.assertIn("SkipprLake:", text)
            self.assertIn(f"catalog_table: {hla_e2e.CATALOG_TABLE}", text)
            self.assertNotIn("type: skippr", text)
            self.assertNotIn(f"table: {hla_e2e.OFFSET_TABLE}", text)
            self.assertNotEqual(hla_e2e.OFFSET_TABLE, hla_e2e.CATALOG_TABLE)
            self.assertIn(f"file://{warehouse}", text)
            self.assertIn("skipprd_el_storage_mode: local", text)
            self.assertNotIn("s3://", text)
            two = Path(tmp) / "two.yml"
            hla_e2e.write_two_pipeline_yml(
                two, events, Path(tmp) / "events-b", warehouse
            )
            two_text = two.read_text(encoding="utf-8")
            self.assertIn(f"catalog_table: {hla_e2e.CATALOG_TABLE}", two_text)
            self.assertNotIn(f"table: {hla_e2e.OFFSET_TABLE}", two_text)

    def test_union_sql_uses_pipeline_namespace(self) -> None:
        self.assertIn("FROM hla_events", hla_e2e.ROW_SQL)
        self.assertIn("FROM hla_events", hla_e2e.COUNT_SQL)

    def test_late_catchup_waits_for_spare_row_not_query_ready(self) -> None:
        src = Path(hla_e2e.__file__).read_text(encoding="utf-8")
        catchup = src[src.index("def scenario_late_catchup") :]
        catchup = catchup[: catchup.index("\ndef scenario_")]
        self.assertIn("wait_membership(harness, ready=2, timeout=90, nodes=3)", catchup)
        self.assertNotIn("ready=3", catchup)

    def test_failpoint_path_is_pipeline_root(self) -> None:
        node = hla_e2e.Node(
            name="n1",
            host="hla-host-1",
            data_dir=Path("/tmp/node1"),
            log_path=Path("/tmp/node1.log"),
        )
        self.assertEqual(
            hla_e2e.failpoint_path(node),
            Path("/tmp/node1/clustered/hla-e2e/local/hla_events/failpoint"),
        )

    def test_before_catalog_commit_scenario_uses_failpoint_and_batch3(self) -> None:
        src = Path(hla_e2e.__file__).read_text(encoding="utf-8")
        self.assertIn("arm_before_catalog_commit", src)
        self.assertIn("wait_node_exit", src)
        self.assertIn("batch3.jsonl", src)
        self.assertTrue((hla_e2e.TESTDATA / "batch3.jsonl").is_file())

    def test_local_commit_and_schema_batches_exist(self) -> None:
        src = Path(hla_e2e.__file__).read_text(encoding="utf-8")
        self.assertIn("after_catalog_commit", src)
        self.assertIn("after_replica_payload_ack", src)
        self.assertIn("after_offsets_published", src)
        self.assertIn("after_compaction_sink", src)
        self.assertIn("hold_before_compaction_sink", src)
        self.assertIn("before_catalog_commit", src)
        self.assertIn('harness.events_dir / "batch12.jsonl"', src)
        self.assertIn("clustered failpoint hold", src)
        self.assertIn("WAL catalog reclaim", src)
        self.assertTrue((hla_e2e.TESTDATA / "batch11.jsonl").is_file())
        self.assertTrue((hla_e2e.TESTDATA / "batch12.jsonl").is_file())
        self.assertTrue((hla_e2e.TESTDATA / "batch14.jsonl").is_file())
        self.assertTrue((hla_e2e.TESTDATA / "batch15.jsonl").is_file())
        self.assertIn("batch4.jsonl", src)
        self.assertTrue((hla_e2e.TESTDATA / "batch4.jsonl").is_file())
        self.assertTrue((hla_e2e.TESTDATA / "batch10.jsonl").is_file())
        self.assertIn("ClusteredSinkNotIdempotent", src)
        self.assertIn("assert_dynamo_closed", src)
        self.assertIn("restart_dead_nodes(harness)", src)
        self.assertIn("wait_live_primary(harness, 90)", src)
        self.assertIn("latest_primary_started_at", src)
        self.assertIn("current_primary", src)
        self.assertIn("process_log", src)
        self.assertIn("wait_assigned_replica(harness, primary, 90)", src)
        self.assertIn("wait_payload_quorum", src)
        self.assertIn("isolate_replica_with_payloads", src)
        self.assertIn("corrupt_replica_payloads", src)
        self.assertIn("purge-seed.jsonl", src)
        self.assertNotIn("query succeeded while initialized heads disagreed", src)
        wait_promoted = src[src.index("def wait_promoted") :]
        wait_promoted = wait_promoted[: wait_promoted.index("\ndef ")]
        self.assertIn("marks", wait_promoted)
        self.assertIn("added", wait_promoted)
        self.assertIn("query_flight_ids", src)
        self.assertIn("query_flight_count", src)
        self.assertIn("QueryRuntime started", src)
        self.assertIn("wait_query_runtime", src)
        self.assertIn("last_flight_addr", src)
        self.assertIn("reap_stale_hla_processes", src)
        self.assertIn("harness.nodes[:2]", src)
        self.assertIn("HLA_VENV = flight_sql.HLA_VENV", src)
        flight = (
            Path(hla_e2e.__file__).resolve().parents[1]
            / "e2e_support"
            / "flight_sql.py"
        ).read_text(encoding="utf-8")
        self.assertIn("pypi.org/simple", flight)
        self.assertIn("hla-venv", flight)
        self.assertIn("PROTOCOL_DUMMY_ADDR", src)
        self.assertNotIn("Iceberg-only", src)
        self.assertIn("must fail closed", src)
        self.assertNotIn("skipped unreachable replica", src)
        self.assertNotIn("query_pin_delays_reclaim", src)
        self.assertNotIn("hold_query_pin", src)
        sigstop = src[src.index("def scenario_sigstop_old_primary") :]
        self.assertIn("restart_dead_nodes(harness)", sigstop)
        self.assertIn("SIGSTOP", sigstop)

    def test_cluster_id_mtls_and_protocol_3_hello(self) -> None:
        src = Path(hla_e2e.__file__).read_text(encoding="utf-8")
        self.assertEqual(hla_e2e.CLUSTER_ID, "hla-lake")
        self.assertEqual(hla_e2e.CURRENT_PROTOCOL, 3)
        self.assertEqual(hla_e2e.PROTOCOL_MIN, 3)
        self.assertEqual(hla_e2e.PROTOCOL_MAX, 3)
        self.assertIn("SKIPPR_CLUSTER_GOSSIP_KEY", src)
        self.assertIn("mint_cluster_tls", src)
        self.assertIn("replica_ssl_context", src)
        self.assertIn("server_hostname=\"skippr-cluster\"", src)
        self.assertIn("cluster#{CLUSTER_ID}", src)
        self.assertIn("put_protocol3_dummy", src)
        hello = hla_e2e.encode_client_hello(
            cluster_id=hla_e2e.CLUSTER_ID, protocol_min=3, protocol_max=3
        )
        self.assertNotIn(b"hla-e2e", hello)
        digest = __import__("hashlib").sha256(b"hla-lake").hexdigest().encode()
        self.assertIn(digest, hello)
        status = hla_e2e.encode_status_request()
        self.assertEqual(status[0], (9 << 3) | 2)
        self.assertNotEqual(status[0], (12 << 3) | 2)

    def test_offset_closed_decodes_le_u64(self) -> None:
        import base64

        payload = bytearray(24)
        payload[16:24] = (1).to_bytes(8, "little")
        self.assertEqual(hla_e2e.offset_closed(base64.b64encode(payload).decode()), 1)

    def test_corrupt_replica_payloads_truncates_seg_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            node = hla_e2e.Node(
                name="n1",
                host="hla-host-1",
                data_dir=Path(tmp) / "node1",
                log_path=Path(tmp) / "n1.log",
            )
            segs = (
                hla_e2e.pipeline_root(node)
                / "segment_buffer"
                / "segs"
            )
            segs.mkdir(parents=True)
            payload = segs / "seg-1.seg"
            payload.write_bytes(b"not-empty")
            hla_e2e.corrupt_replica_payloads(node)
            self.assertEqual(payload.read_bytes(), b"")

    def test_primary_node_picks_latest_ingest_started(self) -> None:
        class Alive:
            def poll(self) -> None:
                return None

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            n1_log = root / "n1.log"
            n2_log = root / "n2.log"
            n1_log.write_text(
                "2026-08-17T08:00:00.000000Z  INFO clustered primary ingest started\n",
                encoding="utf-8",
            )
            n2_log.write_text(
                "2026-08-17T08:10:00.000000Z  INFO clustered primary ingest started\n",
                encoding="utf-8",
            )
            n1 = hla_e2e.Node(
                name="n1",
                host="hla-host-1",
                data_dir=root / "n1",
                log_path=n1_log,
            )
            n2 = hla_e2e.Node(
                name="n2",
                host="hla-host-2",
                data_dir=root / "n2",
                log_path=n2_log,
            )
            n1.process = Alive()
            n2.process = Alive()
            harness = hla_e2e.Harness(
                workdir=root,
                skipprd=root / "skipprd",
                config_path=root / "skippr.yml",
                warehouse=root / "warehouse",
                events_dir=root / "events",
                manifest_dir=str(root),
                keep=False,
                tls_dir=root / "tls",
                nodes=[n1, n2],
            )
            self.assertEqual(hla_e2e.primary_node(harness).name, "n2")

    def test_replica_node_ignores_stale_skipprd_catchup(self) -> None:
        class Alive:
            def poll(self) -> None:
                return None

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            nodes = []
            for idx, extra in (
                (
                    1,
                    "2026-08-17T08:20:00.000000Z  INFO clustered primary ingest started\n",
                ),
                (2, "2026-08-17T08:00:00.000000Z  INFO clustered scheduler started\n"),
                (
                    3,
                    "assigned replica payload catch-up complete\nreplica=127.0.0.1:9\n",
                ),
            ):
                log_path = root / f"n{idx}.log"
                log_path.write_text(extra, encoding="utf-8")
                node = hla_e2e.Node(
                    name=f"n{idx}",
                    host=f"hla-host-{idx}",
                    data_dir=root / f"n{idx}",
                    log_path=log_path,
                )
                node.process = Alive()
                nodes.append(node)
            stale_dir = nodes[1].data_dir / "logs"
            stale_dir.mkdir(parents=True)
            (stale_dir / "skipprd.log").write_text(
                "assigned replica payload catch-up complete\n",
                encoding="utf-8",
            )
            harness = hla_e2e.Harness(
                workdir=root,
                skipprd=root / "skipprd",
                config_path=root / "skippr.yml",
                warehouse=root / "warehouse",
                events_dir=root / "events",
                manifest_dir=str(root),
                keep=False,
                tls_dir=root / "tls",
                nodes=nodes,
            )
            self.assertEqual(hla_e2e.primary_node(harness).name, "n1")
            self.assertEqual(hla_e2e.replica_node(harness).name, "n3")

    def test_primary_ingest_pipelines_dedupes_tracing_copies(self) -> None:
        text = (
            "2026-08-17T08:57:30.646874Z  INFO clustered primary ingest started "
            "pipeline=hla_events_b\n"
            "2026-08-17T08:57:30.646937Z  INFO skipprd::cluster::scheduler: "
            "clustered primary ingest started pipeline=hla_events_b\n"
        )
        self.assertEqual(hla_e2e.primary_ingest_pipelines(text), {"hla_events_b"})

    def test_ddb_cas_cases_match_contract(self) -> None:
        cas_path = Path(__file__).resolve().parent / "ddb_cas.py"
        spec = importlib.util.spec_from_file_location("hla_e2e_ddb_cas", cas_path)
        assert spec is not None and spec.loader is not None
        ddb_cas = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(ddb_cas)
        self.assertEqual(
            ddb_cas.CASES,
            [
                "lease_create",
                "steal_stale_heartbeat",
                "steal_matching_observation",
                "renew_wrong_owner",
                "release_then_acquire_released",
                "membership_delete_stale_heartbeat",
                "membership_matching_heartbeat",
                "offset_create_cas",
                "offset_stale_fence",
                "offset_matching_fence",
                "catalog_pointer_create",
                "catalog_generation_occ",
            ],
        )
        self.assertEqual(list(ddb_cas.CASE_FNS), ddb_cas.CASES)
        self.assertEqual(ddb_cas.OFFSET_TABLE, hla_e2e.OFFSET_TABLE)
        self.assertEqual(ddb_cas.CATALOG_TABLE, hla_e2e.CATALOG_TABLE)
        self.assertNotEqual(ddb_cas.OFFSET_TABLE, ddb_cas.CATALOG_TABLE)
        self.assertEqual(ddb_cas.CLUSTER_PK, f"cluster#{hla_e2e.CLUSTER_ID}")
        src = cas_path.read_text(encoding="utf-8")
        self.assertIn("attribute_not_exists(PK)", src)
        self.assertIn("owner_node = :old_owner AND epoch = :old_epoch", src)
        self.assertIn("AND heartbeat = :old_heartbeat AND released = :released", src)
        self.assertIn(
            "wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h", src
        )
        self.assertIn(
            "attribute_exists(PK) AND generation = :gen AND metadata_location = :loc",
            src,
        )
        self.assertIn("catalog#", src)
        self.assertIn("encode_name", src)
        self.assertIn("TableName=CATALOG_TABLE", src)
        self.assertIn("cluster#", src)
        self.assertNotIn("#{WORKSPACE}#cluster", src)
        catalog_src = src[src.index("def case_catalog_pointer_create") :]
        self.assertNotIn("TableName=OFFSET_TABLE", catalog_src)


if __name__ == "__main__":
    unittest.main()
