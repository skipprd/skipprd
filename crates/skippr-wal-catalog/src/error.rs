use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum CatalogError {
    #[error("catalog transaction is full")]
    TxnFull,
    #[error("catalog row exceeds size ceiling ({bytes} bytes)")]
    RowTooLarge { bytes: usize },
    #[error("catalog commit fenced")]
    Fenced,
    #[error("catalog condition failed")]
    ConditionFailed,
    #[error("catalog transaction conflict")]
    Conflict,
    #[error("catalog codec: {0}")]
    Codec(String),
    #[error("catalog io: {0}")]
    Io(String),
    #[error("SeqAdvance is only valid on CatalogKey::Seq")]
    SeqOpOnNonSeqKey,
    #[error("payload location does not match the configured layer")]
    WrongPayloadLayer,
}
