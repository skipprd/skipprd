use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum CatalogError {
    #[error("catalog commit fenced")]
    Fenced,
    #[error("catalog condition failed")]
    ConditionFailed,
    #[error("catalog put version must be expected plus one")]
    VersionMustAdvance,
    #[error("legacy catalog slice rows are read-only")]
    LegacyRowWrite,
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

#[cfg(test)]
mod tests {
    #[test]
    fn catalog_error_has_no_dynamo_size_ceilings() {
        let src = include_str!("error.rs");
        assert!(!src.contains(concat!("Txn", "Full")));
        assert!(!src.contains(concat!("RowToo", "Large")));
        assert!(!src.contains(concat!("catalog transaction is ", "full")));
        assert!(!src.contains(concat!("catalog row exceeds size ", "ceiling")));
    }
}
