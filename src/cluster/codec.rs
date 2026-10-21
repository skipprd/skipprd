//! Cluster protocol v3 control-frame codec. Payload bytes stream after the frame.

use prost::Message;
use skippr_lease::{DurableError, PipelineKey, CONTROL_FRAME_MAX_BYTES};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/skippr.cluster.v1.rs"));
}

pub fn encode_control(frame: &proto::ControlFrame) -> Result<Vec<u8>, DurableError> {
    let mut buf = Vec::new();
    frame
        .encode(&mut buf)
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))?;
    if buf.len() > CONTROL_FRAME_MAX_BYTES {
        return Err(DurableError::ProtocolMismatch(
            "control frame exceeds 16 MiB".into(),
        ));
    }
    Ok(buf)
}

pub fn decode_control(bytes: &[u8]) -> Result<proto::ControlFrame, DurableError> {
    proto::ControlFrame::decode(bytes)
        .map_err(|err| DurableError::ProtocolMismatch(format!("corrupt protobuf: {err}")))
}

pub fn pipeline_to_proto(key: &PipelineKey) -> proto::PipelineKey {
    proto::PipelineKey {
        tenant: key.tenant().to_string(),
        workspace: key.workspace().to_string(),
        pipeline: key.pipeline().to_string(),
    }
}

pub fn pipeline_from_proto(key: Option<&proto::PipelineKey>) -> Result<PipelineKey, DurableError> {
    let key = key.ok_or_else(|| DurableError::ProtocolMismatch("missing pipeline key".into()))?;
    if key.tenant.is_empty() || key.workspace.is_empty() || key.pipeline.is_empty() {
        return Err(DurableError::ProtocolMismatch(
            "pipeline key components must not be empty".into(),
        ));
    }
    PipelineKey::new(&key.tenant, &key.workspace, &key.pipeline)
        .map_err(|err| DurableError::ProtocolMismatch(err.to_string()))
}

pub fn hash32(bytes: &[u8]) -> Result<[u8; 32], DurableError> {
    if bytes.len() != 32 {
        return Err(DurableError::ProtocolMismatch(format!(
            "expected 32-byte hash, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_roundtrip() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let proto = pipeline_to_proto(&key);
        assert_eq!(pipeline_from_proto(Some(&proto)).unwrap(), key);
    }

    #[test]
    fn empty_pipeline_is_rejected() {
        let err = pipeline_from_proto(Some(&proto::PipelineKey {
            tenant: "t".into(),
            workspace: "".into(),
            pipeline: "p".into(),
        }))
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn hash32_requires_32_bytes() {
        assert!(hash32(&[1u8; 16]).is_err());
        assert_eq!(hash32(&[7u8; 32]).unwrap(), [7u8; 32]);
    }
}
