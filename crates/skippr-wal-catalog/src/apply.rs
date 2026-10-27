use crate::error::CatalogError;
use crate::key::CatalogKey;
use crate::row::{CatalogOp, CatalogRow, DeleteCond, PutCond, SeqRow};
use crate::txn::CatalogTxn;

pub fn validate_put(op: &CatalogOp) -> Result<(), CatalogError> {
    let CatalogOp::Put { row, cond, .. } = op else {
        return Ok(());
    };
    if matches!(row, CatalogRow::LegacySlice(_)) {
        return Err(CatalogError::LegacyRowWrite);
    }
    let PutCond::Version(expected) = cond else {
        return Ok(());
    };
    let next = expected
        .checked_add(1)
        .ok_or(CatalogError::VersionMustAdvance)?;
    match row.version() {
        Some(got) if got == next => Ok(()),
        _ => Err(CatalogError::VersionMustAdvance),
    }
}

pub fn eval_ops(
    txn: &CatalogTxn,
    get: impl Fn(&CatalogKey) -> Result<Option<Vec<u8>>, CatalogError>,
) -> Result<Vec<Applied>, CatalogError> {
    let mut planned = Vec::with_capacity(txn.ops().len());
    for op in txn.ops() {
        validate_put(op)?;
        match op {
            CatalogOp::Put { key, row, cond } => {
                let incoming = row.encode()?;
                let existing = get(key)?;
                match cond {
                    PutCond::NotExists => {
                        if existing.is_some() {
                            return Err(CatalogError::ConditionFailed);
                        }
                    }
                    PutCond::SameSha => {
                        let Some(existing) = existing.as_deref() else {
                            return Err(CatalogError::ConditionFailed);
                        };
                        if CatalogRow::sha256(existing) != CatalogRow::sha256(&incoming) {
                            return Err(CatalogError::ConditionFailed);
                        }
                    }
                    PutCond::Version(expected) => {
                        let Some(existing) = existing.as_deref() else {
                            return Err(CatalogError::ConditionFailed);
                        };
                        if row_version(existing)? != *expected {
                            return Err(CatalogError::ConditionFailed);
                        }
                    }
                    PutCond::SeqAdvance => {
                        if key != &CatalogKey::Seq {
                            return Err(CatalogError::SeqOpOnNonSeqKey);
                        }
                        let CatalogRow::Seq(SeqRow { max_seq }) = row else {
                            return Err(CatalogError::Codec("SeqAdvance requires SeqRow".into()));
                        };
                        match existing.as_deref() {
                            None => {}
                            Some(bytes) => match CatalogRow::decode(bytes)? {
                                CatalogRow::Seq(SeqRow { max_seq: old })
                                    if old.get() < max_seq.get() => {}
                                _ => return Err(CatalogError::ConditionFailed),
                            },
                        }
                    }
                }
                planned.push(Applied::Put {
                    key: key.clone(),
                    bytes: incoming,
                });
            }
            CatalogOp::Delete { key, cond } => {
                let existing = get(key)?;
                match cond {
                    DeleteCond::Exists => {
                        if existing.is_none() {
                            return Err(CatalogError::ConditionFailed);
                        }
                    }
                    DeleteCond::Version(expected) => {
                        let Some(existing) = existing.as_deref() else {
                            return Err(CatalogError::ConditionFailed);
                        };
                        if row_version(existing)? != *expected {
                            return Err(CatalogError::ConditionFailed);
                        }
                    }
                }
                planned.push(Applied::Delete { key: key.clone() });
            }
        }
    }
    Ok(planned)
}

pub enum Applied {
    Put { key: CatalogKey, bytes: Vec<u8> },
    Delete { key: CatalogKey },
}

fn row_version(bytes: &[u8]) -> Result<u64, CatalogError> {
    CatalogRow::decode(bytes)?
        .version()
        .ok_or_else(|| CatalogError::Codec("SeqRow has no version; use SeqAdvance".into()))
}
