use crate::error::CatalogError;
use crate::key::CatalogKey;
use crate::row::{CatalogOp, CatalogRow, DeleteCond, PutCond};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogTxn {
    ops: Vec<CatalogOp>,
}

impl CatalogTxn {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    pub fn push(&mut self, op: CatalogOp) -> Result<(), CatalogError> {
        if matches!(
            op,
            CatalogOp::Put {
                cond: PutCond::SeqAdvance,
                key: CatalogKey::Seq,
                ..
            }
        ) {
            // valid
        } else if matches!(
            op,
            CatalogOp::Put {
                cond: PutCond::SeqAdvance,
                ..
            }
        ) {
            return Err(CatalogError::SeqOpOnNonSeqKey);
        }
        let _ = op.encoded_len()?;
        self.ops.push(op);
        Ok(())
    }

    pub fn ops(&self) -> &[CatalogOp] {
        &self.ops
    }

    pub fn put(
        &mut self,
        key: CatalogKey,
        row: CatalogRow,
        cond: PutCond,
    ) -> Result<(), CatalogError> {
        self.push(CatalogOp::Put { key, row, cond })
    }

    pub fn delete(&mut self, key: CatalogKey, cond: DeleteCond) -> Result<(), CatalogError> {
        self.push(CatalogOp::Delete { key, cond })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::{CatalogSeq, SeqRow};

    #[test]
    fn push_accepts_more_ops_than_the_old_dynamo_txn_ceiling() {
        let mut txn = CatalogTxn::new();
        for i in 0..100 {
            txn.put(
                CatalogKey::Seq,
                CatalogRow::Seq(SeqRow {
                    max_seq: CatalogSeq::new(i as u64 + 1),
                }),
                PutCond::SeqAdvance,
            )
            .unwrap();
        }
        assert_eq!(txn.ops().len(), 100);
    }

    #[test]
    fn catalog_txn_has_no_op_ceiling() {
        let src = include_str!("txn.rs");
        assert!(!src.contains(concat!("TXN_MAX_", "OPS")));
        assert!(!src.contains(concat!("Txn", "Full")));
    }

    #[test]
    fn seq_advance_on_non_seq_key_is_illegal() {
        let mut txn = CatalogTxn::new();
        let err = txn
            .put(
                CatalogKey::Format,
                CatalogRow::Seq(SeqRow {
                    max_seq: CatalogSeq::new(1),
                }),
                PutCond::SeqAdvance,
            )
            .unwrap_err();
        assert_eq!(err, CatalogError::SeqOpOnNonSeqKey);
    }
}
