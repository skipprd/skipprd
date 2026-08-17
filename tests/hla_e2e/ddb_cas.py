#!/usr/bin/env python3
"""Dedicated DynamoDB Local CAS/OCC suite. No skipprd ingest."""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import sys
import uuid
from pathlib import Path
from typing import Any, Callable

from botocore.exceptions import ClientError

MODULE_PATH = Path(__file__).resolve().parent / "run.py"
SPEC = importlib.util.spec_from_file_location("hla_e2e_run", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
hla_e2e = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = hla_e2e
SPEC.loader.exec_module(hla_e2e)

OFFSET_TABLE = hla_e2e.OFFSET_TABLE
CATALOG_TABLE = hla_e2e.CATALOG_TABLE
TENANT = "hla-cas"
WORKSPACE = "local"
PIPELINE = "events"
LEASE_PK = f"{TENANT}#{WORKSPACE}#{PIPELINE}"
CLUSTER_PK = f"cluster#{hla_e2e.CLUSTER_ID}"
OFFSET_PK = LEASE_PK

CASES = [
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
]


class CasError(RuntimeError):
    pass


def log(msg: str) -> None:
    hla_e2e.log(msg)


def is_ccf(err: ClientError) -> bool:
    return err.response.get("Error", {}).get("Code") == "ConditionalCheckFailedException"


def encode_name(name: str) -> str:
    return f"{len(name)}:{name}"


def warehouse_pk(warehouse: str) -> str:
    normalized = warehouse.rstrip("/")
    digest = hashlib.sha256(normalized.encode()).hexdigest()
    return f"catalog#{digest}"


def put_lease(client: Any, owner: str, epoch: int, heartbeat: int, released: bool) -> None:
    client.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": LEASE_PK},
            "SK": {"S": "lease"},
            "owner_node": {"S": owner},
            "epoch": {"N": str(epoch)},
            "heartbeat": {"N": str(heartbeat)},
            "released": {"BOOL": released},
            "initialized": {"BOOL": False},
        },
    )


def steal(
    client: Any,
    observed_owner: str,
    observed_epoch: int,
    observed_heartbeat: int,
    released: bool,
    new_owner: str,
) -> None:
    client.update_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": LEASE_PK}, "SK": {"S": "lease"}},
        ConditionExpression=(
            "owner_node = :old_owner AND epoch = :old_epoch "
            "AND heartbeat = :old_heartbeat AND released = :released"
        ),
        UpdateExpression=(
            "SET owner_node = :new_owner, epoch = epoch + :one, "
            "heartbeat = :one, released = :false"
        ),
        ExpressionAttributeValues={
            ":old_owner": {"S": observed_owner},
            ":old_epoch": {"N": str(observed_epoch)},
            ":old_heartbeat": {"N": str(observed_heartbeat)},
            ":released": {"BOOL": released},
            ":new_owner": {"S": new_owner},
            ":one": {"N": "1"},
            ":false": {"BOOL": False},
        },
    )


def get_lease(client: Any) -> dict[str, Any]:
    item = client.get_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": LEASE_PK}, "SK": {"S": "lease"}},
        ConsistentRead=True,
    ).get("Item")
    if not item:
        raise CasError("lease item missing")
    return item


def case_lease_create(client: Any) -> None:
    owner = str(uuid.uuid4())
    client.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": LEASE_PK},
            "SK": {"S": "lease"},
            "owner_node": {"S": owner},
            "epoch": {"N": "1"},
            "heartbeat": {"N": "1"},
            "released": {"BOOL": False},
            "initialized": {"BOOL": False},
        },
        ConditionExpression="attribute_not_exists(PK)",
    )
    try:
        client.put_item(
            TableName=OFFSET_TABLE,
            Item={
                "PK": {"S": LEASE_PK},
                "SK": {"S": "lease"},
                "owner_node": {"S": str(uuid.uuid4())},
                "epoch": {"N": "1"},
                "heartbeat": {"N": "1"},
                "released": {"BOOL": False},
                "initialized": {"BOOL": False},
            },
            ConditionExpression="attribute_not_exists(PK)",
        )
    except ClientError as err:
        if is_ccf(err):
            return
        raise
    raise CasError("duplicate lease create did not CCF")


def case_steal_stale_heartbeat(client: Any) -> None:
    owner = str(uuid.uuid4())
    thief = str(uuid.uuid4())
    put_lease(client, owner, 1, 4, False)
    try:
        steal(client, owner, 1, 3, False, thief)
    except ClientError as err:
        if not is_ccf(err):
            raise
    else:
        raise CasError("stale heartbeat steal succeeded")
    item = get_lease(client)
    if item["owner_node"]["S"] != owner or item["epoch"]["N"] != "1":
        raise CasError("stale steal mutated the lease")


def case_steal_matching_observation(client: Any) -> None:
    owner = str(uuid.uuid4())
    thief = str(uuid.uuid4())
    put_lease(client, owner, 2, 5, False)
    steal(client, owner, 2, 5, False, thief)
    item = get_lease(client)
    if item["owner_node"]["S"] != thief:
        raise CasError("matching steal did not take ownership")
    if item["epoch"]["N"] != "3" or item["heartbeat"]["N"] != "1":
        raise CasError(
            f"matching steal expected epoch+1 heartbeat=1, got epoch={item['epoch']['N']} hb={item['heartbeat']['N']}"
        )
    if item["released"]["BOOL"]:
        raise CasError("matching steal left released=true")


def case_renew_wrong_owner(client: Any) -> None:
    owner = str(uuid.uuid4())
    put_lease(client, owner, 1, 1, False)
    try:
        client.update_item(
            TableName=OFFSET_TABLE,
            Key={"PK": {"S": LEASE_PK}, "SK": {"S": "lease"}},
            ConditionExpression="owner_node = :owner AND epoch = :epoch AND released = :false",
            UpdateExpression="SET heartbeat = heartbeat + :one",
            ExpressionAttributeValues={
                ":owner": {"S": str(uuid.uuid4())},
                ":epoch": {"N": "1"},
                ":false": {"BOOL": False},
                ":one": {"N": "1"},
            },
        )
    except ClientError as err:
        if is_ccf(err):
            return
        raise
    raise CasError("renew with wrong owner did not CCF")


def case_release_then_acquire_released(client: Any) -> None:
    owner = str(uuid.uuid4())
    next_owner = str(uuid.uuid4())
    put_lease(client, owner, 4, 8, False)
    client.update_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": LEASE_PK}, "SK": {"S": "lease"}},
        ConditionExpression="owner_node = :owner AND epoch = :epoch AND released = :false",
        UpdateExpression="SET released = :true, heartbeat = heartbeat + :one",
        ExpressionAttributeValues={
            ":owner": {"S": owner},
            ":epoch": {"N": "4"},
            ":false": {"BOOL": False},
            ":true": {"BOOL": True},
            ":one": {"N": "1"},
        },
    )
    steal(client, owner, 4, 9, True, next_owner)
    item = get_lease(client)
    if item["owner_node"]["S"] != next_owner or item["epoch"]["N"] != "5":
        raise CasError("acquire_released did not bump epoch")
    if item["heartbeat"]["N"] != "1" or item["released"]["BOOL"]:
        raise CasError("acquire_released did not reset heartbeat/released")


def put_member(client: Any, node: str, heartbeat: int) -> None:
    client.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": CLUSTER_PK},
            "SK": {"S": f"node#{node}"},
            "host_id": {"S": "hla-cas-host"},
            "heartbeat": {"N": str(heartbeat)},
            "ready": {"BOOL": True},
        },
    )


def case_membership_delete_stale_heartbeat(client: Any) -> None:
    node = str(uuid.uuid4())
    put_member(client, node, 7)
    try:
        client.delete_item(
            TableName=OFFSET_TABLE,
            Key={"PK": {"S": CLUSTER_PK}, "SK": {"S": f"node#{node}"}},
            ConditionExpression="heartbeat = :h",
            ExpressionAttributeValues={":h": {"N": "6"}},
        )
    except ClientError as err:
        if not is_ccf(err):
            raise
    else:
        raise CasError("stale membership delete succeeded")
    item = client.get_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": CLUSTER_PK}, "SK": {"S": f"node#{node}"}},
        ConsistentRead=True,
    ).get("Item")
    if item is None:
        raise CasError("stale membership delete removed the item; Rust maps CCF to Ok and keeps it")


def case_membership_matching_heartbeat(client: Any) -> None:
    node = str(uuid.uuid4())
    put_member(client, node, 7)
    client.delete_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": CLUSTER_PK}, "SK": {"S": f"node#{node}"}},
        ConditionExpression="heartbeat = :h",
        ExpressionAttributeValues={":h": {"N": "7"}},
    )
    item = client.get_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": CLUSTER_PK}, "SK": {"S": f"node#{node}"}},
        ConsistentRead=True,
    ).get("Item")
    if item is not None:
        raise CasError("matching heartbeat delete left the membership item")


def sha256_hex(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def put_offset(client: Any, sk: str, payload: bytes, epoch: int, index: int) -> None:
    client.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": OFFSET_PK},
            "SK": {"S": sk},
            "payload_b64": {"S": base64.b64encode(payload).decode()},
            "payload_sha256": {"S": sha256_hex(payload)},
            "wal_epoch": {"N": str(epoch)},
            "wal_commit_index": {"N": str(index)},
        },
    )


def case_offset_create_cas(client: Any) -> None:
    sk = "offset#events#p0"
    payload = b"\x00" * 24
    client.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": OFFSET_PK},
            "SK": {"S": sk},
            "payload_b64": {"S": base64.b64encode(payload).decode()},
            "payload_sha256": {"S": sha256_hex(payload)},
            "wal_epoch": {"N": "1"},
            "wal_commit_index": {"N": "1"},
        },
        ConditionExpression="attribute_not_exists(PK)",
    )
    try:
        client.put_item(
            TableName=OFFSET_TABLE,
            Item={
                "PK": {"S": OFFSET_PK},
                "SK": {"S": sk},
                "payload_b64": {"S": base64.b64encode(payload).decode()},
                "payload_sha256": {"S": sha256_hex(payload)},
                "wal_epoch": {"N": "2"},
                "wal_commit_index": {"N": "2"},
            },
            ConditionExpression="attribute_not_exists(PK)",
        )
    except ClientError as err:
        if is_ccf(err):
            return
        raise
    raise CasError("duplicate offset create did not CCF")


def case_offset_stale_fence(client: Any) -> None:
    sk = "offset#events#p1"
    payload = b"\x01" * 24
    put_offset(client, sk, payload, 3, 9)
    newer = b"\x02" * 24
    try:
        client.put_item(
            TableName=OFFSET_TABLE,
            Item={
                "PK": {"S": OFFSET_PK},
                "SK": {"S": sk},
                "payload_b64": {"S": base64.b64encode(newer).decode()},
                "payload_sha256": {"S": sha256_hex(newer)},
                "wal_epoch": {"N": "4"},
                "wal_commit_index": {"N": "10"},
            },
            ConditionExpression="wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h",
            ExpressionAttributeValues={
                ":e": {"N": "2"},
                ":i": {"N": "8"},
                ":h": {"S": sha256_hex(payload)},
            },
        )
    except ClientError as err:
        if not is_ccf(err):
            raise
    else:
        raise CasError("stale offset fence write succeeded")
    item = client.get_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": OFFSET_PK}, "SK": {"S": sk}},
        ConsistentRead=True,
    )["Item"]
    if item["wal_epoch"]["N"] != "3" or item["wal_commit_index"]["N"] != "9":
        raise CasError("stale fence mutated the offset")


def case_offset_matching_fence(client: Any) -> None:
    sk = "offset#events#p2"
    payload = b"\x03" * 24
    put_offset(client, sk, payload, 3, 9)
    newer = b"\x04" * 24
    client.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": OFFSET_PK},
            "SK": {"S": sk},
            "payload_b64": {"S": base64.b64encode(newer).decode()},
            "payload_sha256": {"S": sha256_hex(newer)},
            "wal_epoch": {"N": "4"},
            "wal_commit_index": {"N": "10"},
        },
        ConditionExpression="wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h",
        ExpressionAttributeValues={
            ":e": {"N": "3"},
            ":i": {"N": "9"},
            ":h": {"S": sha256_hex(payload)},
        },
    )
    item = client.get_item(
        TableName=OFFSET_TABLE,
        Key={"PK": {"S": OFFSET_PK}, "SK": {"S": sk}},
        ConsistentRead=True,
    )["Item"]
    if item["wal_epoch"]["N"] != "4" or item["wal_commit_index"]["N"] != "10":
        raise CasError("matching fence did not advance")


def catalog_keys() -> tuple[str, str]:
    pk = f"{warehouse_pk('file:///tmp/hla-cas-wh')}#namespace#{encode_name('db')}"
    sk = f"table#{encode_name('t')}"
    return pk, sk


def case_catalog_pointer_create(client: Any) -> None:
    pk, sk = catalog_keys()
    client.put_item(
        TableName=CATALOG_TABLE,
        Item={
            "PK": {"S": pk},
            "SK": {"S": sk},
            "metadata_location": {"S": "file:///tmp/hla-cas-wh/t/metadata/v1.json"},
            "generation": {"N": "1"},
        },
        ConditionExpression="attribute_not_exists(PK)",
    )
    try:
        client.put_item(
            TableName=CATALOG_TABLE,
            Item={
                "PK": {"S": pk},
                "SK": {"S": sk},
                "metadata_location": {"S": "file:///tmp/hla-cas-wh/t/metadata/v2.json"},
                "generation": {"N": "1"},
            },
            ConditionExpression="attribute_not_exists(PK)",
        )
    except ClientError as err:
        if is_ccf(err):
            return
        raise
    raise CasError("duplicate catalog pointer create did not CCF")


def case_catalog_generation_occ(client: Any) -> None:
    pk, sk = catalog_keys()
    loc = "file:///tmp/hla-cas-wh/t/metadata/v1.json"
    new_loc = "file:///tmp/hla-cas-wh/t/metadata/v2.json"
    client.put_item(
        TableName=CATALOG_TABLE,
        Item={
            "PK": {"S": pk},
            "SK": {"S": sk},
            "metadata_location": {"S": loc},
            "generation": {"N": "1"},
        },
    )
    client.update_item(
        TableName=CATALOG_TABLE,
        Key={"PK": {"S": pk}, "SK": {"S": sk}},
        ConditionExpression="attribute_exists(PK) AND generation = :gen AND metadata_location = :loc",
        UpdateExpression=(
            "SET generation = generation + :one, metadata_location = :new, "
            "previous_metadata_location = :loc"
        ),
        ExpressionAttributeValues={
            ":gen": {"N": "1"},
            ":loc": {"S": loc},
            ":new": {"S": new_loc},
            ":one": {"N": "1"},
        },
    )
    try:
        client.update_item(
            TableName=CATALOG_TABLE,
            Key={"PK": {"S": pk}, "SK": {"S": sk}},
            ConditionExpression="attribute_exists(PK) AND generation = :gen AND metadata_location = :loc",
            UpdateExpression=(
                "SET generation = generation + :one, metadata_location = :new, "
                "previous_metadata_location = :loc"
            ),
            ExpressionAttributeValues={
                ":gen": {"N": "1"},
                ":loc": {"S": loc},
                ":new": {"S": "file:///tmp/hla-cas-wh/t/metadata/loser.json"},
                ":one": {"N": "1"},
            },
        )
    except ClientError as err:
        if not is_ccf(err):
            raise
    else:
        raise CasError("losing catalog generation write did not CCF")
    item = client.get_item(
        TableName=CATALOG_TABLE,
        Key={"PK": {"S": pk}, "SK": {"S": sk}},
        ConsistentRead=True,
    )["Item"]
    if item["generation"]["N"] != "2":
        raise CasError(f"catalog OCC winner generation is {item['generation']['N']}, expected 2")
    if item["metadata_location"]["S"] != new_loc:
        raise CasError("catalog OCC winner location was overwritten")


CASE_FNS: dict[str, Callable[[Any], None]] = {
    "lease_create": case_lease_create,
    "steal_stale_heartbeat": case_steal_stale_heartbeat,
    "steal_matching_observation": case_steal_matching_observation,
    "renew_wrong_owner": case_renew_wrong_owner,
    "release_then_acquire_released": case_release_then_acquire_released,
    "membership_delete_stale_heartbeat": case_membership_delete_stale_heartbeat,
    "membership_matching_heartbeat": case_membership_matching_heartbeat,
    "offset_create_cas": case_offset_create_cas,
    "offset_stale_fence": case_offset_stale_fence,
    "offset_matching_fence": case_offset_matching_fence,
    "catalog_pointer_create": case_catalog_pointer_create,
    "catalog_generation_occ": case_catalog_generation_occ,
}


def run() -> None:
    if list(CASE_FNS) != CASES:
        raise CasError("CASE_FNS keys drifted from CASES")
    client = hla_e2e.start_dynamodb()
    try:
        for name in CASES:
            log(f"ddb_cas {name}")
            CASE_FNS[name](client)
        log("ddb_cas passed: " + ", ".join(CASES))
    finally:
        hla_e2e.stop_dynamodb()


def main() -> int:
    try:
        run()
    except (CasError, hla_e2e.HarnessError) as err:
        log(str(err))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
