use serde_json::{json, Value};
use skippr_cloud::{attr_s, bflag, n, s, Client};
use skippr_lease::{LEASE_ATTR_EPOCH, LEASE_ATTR_OWNER, LEASE_ATTR_RELEASED, LEASE_SK};
use skippr_wal_catalog::{
    CatalogError, CatalogFence, CatalogKey, CatalogOp, CatalogPrefix, CatalogRow, CatalogTxn,
    DeleteCond, PutCond, WalCatalogStore, TXN_MAX_OPS,
};

use crate::run_on_worker;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

pub struct CloudTablesWalCatalog {
    table: String,
    pk: String,
}

impl CloudTablesWalCatalog {
    pub fn new(table: String, pk: String) -> Self {
        Self { table, pk }
    }
}

impl WalCatalogStore for CloudTablesWalCatalog {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let sk = key.encode();
        run_on_worker(move |rt, client| rt.block_on(get_row(&client, &table, &pk, &sk)))
            .map_err(|err| CatalogError::Io(err))?
    }

    fn query_prefix(
        &self,
        prefix: CatalogPrefix,
        after: Option<&CatalogKey>,
        limit: usize,
    ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let start = after.map(CatalogKey::encode);
        let p = prefix.as_str().to_string();
        run_on_worker(move |rt, client| {
            rt.block_on(query_prefix(
                &client,
                &table,
                &pk,
                &p,
                start.as_deref(),
                limit,
            ))
        })
        .map_err(|err| CatalogError::Io(err))?
    }

    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let ops = txn.ops().to_vec();
        run_on_worker(move |rt, client| rt.block_on(commit_txn(&client, &table, &pk, &ops, fence)))
            .map_err(|err| CatalogError::Io(err))?
    }

    fn delete_prefix(
        &self,
        prefix: CatalogPrefix,
        fence: CatalogFence,
    ) -> Result<(), CatalogError> {
        loop {
            let page = self.query_prefix(prefix, None, TXN_MAX_OPS)?;
            if page.is_empty() {
                return Ok(());
            }
            let mut txn = CatalogTxn::new();
            for (key, _) in page {
                txn.delete(key, DeleteCond::Exists)?;
            }
            self.commit(&txn, fence)?;
        }
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        Ok(())
    }
}

async fn get_row(
    client: &Client,
    table: &str,
    pk: &str,
    sk: &str,
) -> Result<Option<CatalogRow>, CatalogError> {
    let item = client
        .get_item(table, pk, sk, true)
        .await
        .map_err(|err| CatalogError::Io(err.to_string()))?;
    match item {
        Some(item) => decode_item(&item),
        None => Ok(None),
    }
}

async fn query_prefix(
    client: &Client,
    table: &str,
    pk: &str,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
    let mut out = Vec::new();
    let mut start_sk: Option<String> = None;
    loop {
        let mut body = json!({
            "tableName": table,
            "keyConditionExpression": "PK = :pk AND begins_with(SK, :p)",
            "expressionAttributeValues": {
                ":pk": s(pk),
                ":p": s(prefix),
            },
            "scanIndexForward": true,
            "consistentRead": true,
        });
        if let Some(sk) = start_sk.as_ref() {
            body["exclusiveStartKey"] = json!({
                "PK": s(pk),
                "SK": s(sk),
            });
        }
        let payload = client
            .call_json("tables", "Query", body)
            .await
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        let items = payload
            .body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in items {
            let sk = attr_s(&item, "SK")
                .ok_or_else(|| CatalogError::Codec("catalog item missing SK".into()))?;
            if let Some(after) = after {
                if sk.as_str() <= after {
                    continue;
                }
            }
            let key =
                CatalogKey::decode(&sk).map_err(|err| CatalogError::Codec(err.to_string()))?;
            let row = decode_item(&item)?
                .ok_or_else(|| CatalogError::Codec("catalog query item missing payload".into()))?;
            out.push((key, row));
            if out.len() >= limit {
                return Ok(out);
            }
        }
        start_sk = payload
            .body
            .get("lastEvaluatedKey")
            .and_then(|v| attr_s(v, "SK"));
        if start_sk.is_none() {
            break;
        }
    }
    Ok(out)
}

async fn commit_txn(
    client: &Client,
    table: &str,
    pk: &str,
    ops: &[CatalogOp],
    fence: CatalogFence,
) -> Result<(), CatalogError> {
    let mut items = Vec::with_capacity(ops.len() + 1);
    items.push(lease_check(table, pk, fence));
    for op in ops {
        items.push(op_to_item(table, pk, op)?);
    }
    client
        .transact_write(Value::Array(items))
        .await
        .map_err(map_transact_err)?;
    Ok(())
}

fn lease_check(table: &str, pk: &str, fence: CatalogFence) -> Value {
    json!({
        "conditionCheck": {
            "tableName": table,
            "key": {
                "PK": s(pk),
                "SK": s(LEASE_SK),
            },
            "conditionExpression": format!(
                "{LEASE_ATTR_OWNER} = :owner AND {LEASE_ATTR_EPOCH} = :epoch AND {LEASE_ATTR_RELEASED} = :false"
            ),
            "expressionAttributeValues": {
                ":owner": s(fence.owner.to_string()),
                ":epoch": n(fence.epoch.get()),
                ":false": bflag(false),
            }
        }
    })
}

fn op_to_item(table: &str, pk: &str, op: &CatalogOp) -> Result<Value, CatalogError> {
    match op {
        CatalogOp::Put { key, row, cond } => {
            let bytes = row.encode()?;
            let sha = crate::sha256_hex(&bytes);
            let mut item = json!({
                "PK": s(pk),
                "SK": s(key.encode()),
                "payload_b64": s(B64.encode(&bytes)),
                "payload_sha256": s(sha.clone()),
            });
            if let CatalogRow::Seq(seq) = row {
                item["max_seq"] = n(seq.max_seq.get());
            }
            if let Some(version) = row_version(row) {
                item["version"] = n(version);
            }
            let (condition, values) = match cond {
                PutCond::NotExists => ("attribute_not_exists(SK)".to_string(), None),
                PutCond::SameSha => (
                    "payload_sha256 = :h".to_string(),
                    Some(json!({ ":h": s(sha) })),
                ),
                PutCond::Version(v) => ("version = :v".to_string(), Some(json!({ ":v": n(*v) }))),
                PutCond::SeqAdvance => (
                    "attribute_not_exists(SK) OR max_seq < :n".to_string(),
                    Some(json!({ ":n": n(seq_value(row)?) })),
                ),
            };
            let mut put = json!({
                "put": {
                    "tableName": table,
                    "item": item,
                    "conditionExpression": condition,
                }
            });
            if let Some(values) = values {
                put["put"]["expressionAttributeValues"] = values;
            }
            Ok(put)
        }
        CatalogOp::Delete { key, cond } => {
            let (condition, values) = match cond {
                DeleteCond::Exists => ("attribute_exists(SK)".to_string(), None),
                DeleteCond::Version(v) => (
                    "attribute_exists(SK) AND version = :v".to_string(),
                    Some(json!({ ":v": n(*v) })),
                ),
            };
            let mut del = json!({
                "delete": {
                    "tableName": table,
                    "key": {
                        "PK": s(pk),
                        "SK": s(key.encode()),
                    },
                    "conditionExpression": condition,
                }
            });
            if let Some(values) = values {
                del["delete"]["expressionAttributeValues"] = values;
            }
            Ok(del)
        }
    }
}

fn seq_value(row: &CatalogRow) -> Result<u64, CatalogError> {
    match row {
        CatalogRow::Seq(seq) => Ok(seq.max_seq.get()),
        _ => Err(CatalogError::SeqOpOnNonSeqKey),
    }
}

fn row_version(row: &CatalogRow) -> Option<u64> {
    match row {
        CatalogRow::Segment(r) => Some(r.version),
        CatalogRow::Compaction(r) => Some(r.version),
        CatalogRow::Control(r) => Some(r.version),
        CatalogRow::Format(r) => Some(r.row_version),
        CatalogRow::Seq(_) => None,
    }
}

fn decode_item(item: &Value) -> Result<Option<CatalogRow>, CatalogError> {
    let Some(payload) = attr_s(item, "payload_b64") else {
        return Ok(None);
    };
    let bytes = B64
        .decode(payload)
        .map_err(|err| CatalogError::Codec(err.to_string()))?;
    Ok(Some(CatalogRow::decode(&bytes)?))
}

#[cfg(test)]
mod tests {
    use skippr_wal_catalog::WalCatalogStore;

    #[test]
    fn cloud_tables_wal_catalog_is_wal_catalog_store() {
        fn assert_store<T: WalCatalogStore>() {}
        assert_store::<super::CloudTablesWalCatalog>();
    }
}

fn map_transact_err(err: skippr_cloud::Error) -> CatalogError {
    let msg = err.to_string();
    if err.is_conditional_check_failed()
        || msg.contains("ConditionalCheckFailed")
        || msg.contains("TransactionCanceled")
    {
        CatalogError::ConditionFailed
    } else if msg.contains("TransactionConflict") {
        CatalogError::Conflict
    } else {
        CatalogError::Io(msg)
    }
}
