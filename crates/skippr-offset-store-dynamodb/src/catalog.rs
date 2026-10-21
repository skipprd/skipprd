use std::collections::HashMap;

use aws_sdk_dynamodb::types::{AttributeValue, ConditionCheck, Delete, Put, TransactWriteItem};
use aws_sdk_dynamodb::Client;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use skippr_lease::{LEASE_ATTR_EPOCH, LEASE_ATTR_OWNER, LEASE_ATTR_RELEASED, LEASE_SK};
use skippr_wal_catalog::{
    CatalogError, CatalogFence, CatalogKey, CatalogOp, CatalogPrefix, CatalogRow, CatalogTxn,
    DeleteCond, PutCond, WalCatalogStore, TXN_MAX_OPS,
};

use crate::run_on_worker;

pub struct DynamoWalCatalog {
    table: String,
    pk: String,
}

impl DynamoWalCatalog {
    pub fn new(table: String, pk: String) -> Self {
        Self { table, pk }
    }
}

impl WalCatalogStore for DynamoWalCatalog {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let sk = key.encode();
        run_on_worker(move |rt, client| rt.block_on(get_row(&client, &table, &pk, &sk)))
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
    }

    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let ops = txn.ops().to_vec();
        run_on_worker(move |rt, client| rt.block_on(commit_txn(&client, &table, &pk, &ops, fence)))
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
    let out = client
        .get_item()
        .table_name(table)
        .key("PK", AttributeValue::S(pk.to_string()))
        .key("SK", AttributeValue::S(sk.to_string()))
        .consistent_read(true)
        .send()
        .await
        .map_err(|err| CatalogError::Io(err.to_string()))?;
    let Some(item) = out.item else {
        return Ok(None);
    };
    decode_item(&item)
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
    let mut start_key = None;
    loop {
        let mut q = client
            .query()
            .table_name(table)
            .key_condition_expression("PK = :pk AND begins_with(SK, :p)")
            .expression_attribute_values(":pk", AttributeValue::S(pk.to_string()))
            .expression_attribute_values(":p", AttributeValue::S(prefix.to_string()))
            .consistent_read(true);
        if let Some(key) = start_key.take() {
            q = q.set_exclusive_start_key(Some(key));
        }
        let page = q
            .send()
            .await
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        for item in page.items() {
            let sk = item
                .get("SK")
                .and_then(|v| v.as_s().ok())
                .ok_or_else(|| CatalogError::Codec("catalog item missing SK".into()))?;
            if let Some(after) = after {
                if sk.as_str() <= after {
                    continue;
                }
            }
            let key = CatalogKey::decode(sk).map_err(|err| CatalogError::Codec(err.to_string()))?;
            let row = decode_item(item)?
                .ok_or_else(|| CatalogError::Codec("catalog query item missing payload".into()))?;
            out.push((key, row));
            if out.len() >= limit {
                return Ok(out);
            }
        }
        start_key = page.last_evaluated_key().cloned();
        if start_key.is_none() {
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
    items.push(
        TransactWriteItem::builder()
            .condition_check(lease_check(table, pk, fence)?)
            .build(),
    );
    for op in ops {
        items.push(op_to_item(table, pk, op)?);
    }
    client
        .transact_write_items()
        .set_transact_items(Some(items))
        .send()
        .await
        .map_err(map_transact_err)?;
    Ok(())
}

fn lease_check(table: &str, pk: &str, fence: CatalogFence) -> Result<ConditionCheck, CatalogError> {
    ConditionCheck::builder()
        .table_name(table)
        .key("PK", AttributeValue::S(pk.to_string()))
        .key("SK", AttributeValue::S(LEASE_SK.to_string()))
        .condition_expression(format!(
            "{LEASE_ATTR_OWNER} = :owner AND {LEASE_ATTR_EPOCH} = :epoch AND {LEASE_ATTR_RELEASED} = :false"
        ))
        .expression_attribute_values(
            ":owner",
            AttributeValue::S(fence.owner.to_string()),
        )
        .expression_attribute_values(
            ":epoch",
            AttributeValue::N(fence.epoch.get().to_string()),
        )
        .expression_attribute_values(":false", AttributeValue::Bool(false))
        .build()
        .map_err(|err| CatalogError::Io(err.to_string()))
}

fn op_to_item(table: &str, pk: &str, op: &CatalogOp) -> Result<TransactWriteItem, CatalogError> {
    match op {
        CatalogOp::Put { key, row, cond } => {
            let bytes = row.encode()?;
            let sha = hex_sha(&bytes);
            let mut item = HashMap::new();
            item.insert("PK".into(), AttributeValue::S(pk.to_string()));
            item.insert("SK".into(), AttributeValue::S(key.encode()));
            item.insert("payload_b64".into(), AttributeValue::S(B64.encode(&bytes)));
            item.insert("payload_sha256".into(), AttributeValue::S(sha.clone()));
            if let CatalogRow::Seq(seq) = row {
                item.insert(
                    "max_seq".into(),
                    AttributeValue::N(seq.max_seq.get().to_string()),
                );
            }
            if let Some(version) = row_version(row) {
                item.insert("version".into(), AttributeValue::N(version.to_string()));
            }
            let mut put = Put::builder().table_name(table).set_item(Some(item));
            put = match cond {
                PutCond::NotExists => put.condition_expression("attribute_not_exists(SK)"),
                PutCond::SameSha => put
                    .condition_expression("payload_sha256 = :h")
                    .expression_attribute_values(":h", AttributeValue::S(sha)),
                PutCond::Version(v) => put
                    .condition_expression("version = :v")
                    .expression_attribute_values(":v", AttributeValue::N(v.to_string())),
                PutCond::SeqAdvance => put
                    .condition_expression("attribute_not_exists(SK) OR max_seq < :n")
                    .expression_attribute_values(
                        ":n",
                        AttributeValue::N(seq_value(row)?.to_string()),
                    ),
            };
            Ok(TransactWriteItem::builder()
                .put(
                    put.build()
                        .map_err(|err| CatalogError::Io(err.to_string()))?,
                )
                .build())
        }
        CatalogOp::Delete { key, cond } => {
            let mut del = Delete::builder()
                .table_name(table)
                .key("PK", AttributeValue::S(pk.to_string()))
                .key("SK", AttributeValue::S(key.encode()));
            del = match cond {
                DeleteCond::Exists => del.condition_expression("attribute_exists(SK)"),
                DeleteCond::Version(v) => del
                    .condition_expression("attribute_exists(SK) AND version = :v")
                    .expression_attribute_values(":v", AttributeValue::N(v.to_string())),
            };
            Ok(TransactWriteItem::builder()
                .delete(
                    del.build()
                        .map_err(|err| CatalogError::Io(err.to_string()))?,
                )
                .build())
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

fn decode_item(item: &HashMap<String, AttributeValue>) -> Result<Option<CatalogRow>, CatalogError> {
    let Some(payload) = item.get("payload_b64").and_then(|v| v.as_s().ok()) else {
        return Ok(None);
    };
    let bytes = B64
        .decode(payload)
        .map_err(|err| CatalogError::Codec(err.to_string()))?;
    Ok(Some(CatalogRow::decode(&bytes)?))
}

fn hex_sha(bytes: &[u8]) -> String {
    crate::sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use skippr_wal_catalog::WalCatalogStore;

    #[test]
    fn dynamo_wal_catalog_is_wal_catalog_store() {
        fn assert_store<T: WalCatalogStore>() {}
        assert_store::<super::DynamoWalCatalog>();
    }
}

fn map_transact_err(
    err: aws_sdk_dynamodb::error::SdkError<
        aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError,
    >,
) -> CatalogError {
    let msg = err.to_string();
    if msg.contains("TransactionConflict") {
        CatalogError::Conflict
    } else if msg.contains("ConditionalCheckFailed") || msg.contains("TransactionCanceled") {
        CatalogError::ConditionFailed
    } else {
        CatalogError::Io(msg)
    }
}
