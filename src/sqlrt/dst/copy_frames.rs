use crate::pipeline_type::PipelineType;
use crate::runtime_plugins::protocol::{HostFrame, PluginFrame};
use crate::runtime_plugins::wire::decode_payload;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct PipelineTypeWire {
    #[serde(with = "crate::pipeline_type::wire")]
    pipeline_type: PipelineType,
}

#[test]
fn d_installed_four_byte_unit() {
    let bytes = bincode::serialize(&PluginFrame::Installed).unwrap();
    assert_eq!(bytes.len(), 4);
    let decoded: PluginFrame = decode_payload(&bytes).unwrap();
    assert!(matches!(decoded, PluginFrame::Installed));
}

#[test]
fn d_trailing_bincode_rejected() {
    let mut bytes = bincode::serialize(&HostFrame::Shutdown).unwrap();
    bytes.extend_from_slice(&[0, 1, 2, 3]);
    let err = decode_payload::<HostFrame>(&bytes).unwrap_err();
    assert!(err.to_string().contains("prefix="), "{err}");
}

#[test]
fn d_handshake_prepare_sink_rawbytes() {
    use crate::buffer::compaction_transaction::SinkWriteSemantics;
    use crate::runtime_plugins::protocol::{
        PrepareSink, RuntimeBinding, RuntimeSinkPayloadMode, SinkRunRequest,
    };
    let handshake = bincode::serialize(&HostFrame::Shutdown).unwrap();
    let decoded: HostFrame = decode_payload(&handshake).unwrap();
    assert!(matches!(decoded, HostFrame::Shutdown));
    let request = SinkRunRequest {
        request_id: 31,
        compaction_id: "c31".into(),
        idempotency_key: "k31".into(),
        wal_refs: Vec::new(),
        write_semantics: SinkWriteSemantics::AtLeastOnce,
        schema_fingerprint: String::new(),
        binding: RuntimeBinding::Primary,
        required_schema_namespace: String::new(),
        required_schema_version: 0,
        filename: "cube.json".into(),
        cdc_ctx: None,
        source_contract: None,
        payload_mode: RuntimeSinkPayloadMode::RawBytes,
        live_wal_segments: None,
    };
    let prepare = PrepareSink {
        request_id: 31,
        envelope: crate::sink_apply_identity::SinkApplyEnvelopeV2::new(
            request.compaction_id.clone(),
            request.idempotency_key.clone(),
            &request.wal_refs,
            false,
            Default::default(),
            request.write_semantics,
            None,
            None,
            None,
        ),
        request,
    };
    let payload = bincode::serialize(&HostFrame::PrepareSink(prepare)).unwrap();
    assert_eq!(u32::from_le_bytes(payload[0..4].try_into().unwrap()), 6);
    let decoded: HostFrame = decode_payload(&payload).unwrap();
    let HostFrame::PrepareSink(got) = decoded else {
        panic!("expected PrepareSink");
    };
    assert_eq!(got.request.payload_mode, RuntimeSinkPayloadMode::RawBytes);
    let installed = bincode::serialize(&PluginFrame::Installed).unwrap();
    let plugin: PluginFrame = decode_payload(&installed).unwrap();
    assert!(matches!(plugin, PluginFrame::Installed));
    let err = decode_payload::<HostFrame>(&[6, 0, 0, 0, 0, 0]).unwrap_err();
    assert!(err.to_string().contains("prefix="), "{err}");
}

#[test]
fn d_sink_pool_does_not_hold_notify_across_grow() {
    assert!(
        !crate::runtime_plugins::host::sink_pool_subscribes_after_grow(true),
        "grow continue must not subscribe"
    );
    assert!(
        crate::runtime_plugins::host::sink_pool_subscribes_after_grow(false),
        "wait must subscribe after grow returns false"
    );
}

#[test]
fn d_copy_grouped_timeout_is_900s() {
    use crate::buffer::ingest_buffer::Buffers;
    assert_eq!(Buffers::GROUPED_COMPACTION_TIMEOUT_SECS, 900);
    assert_eq!(Buffers::grouped_compaction_timeout().as_secs(), 900);
}

#[test]
fn d_protocol_22_pipeline_type() {
    for ty in PipelineType::ALL {
        let bytes = bincode::serialize(&PipelineTypeWire { pipeline_type: ty }).unwrap();
        let back: PipelineTypeWire = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.pipeline_type, ty);
    }
    let leftover = bincode::serialize(&"copy".to_string()).unwrap();
    assert!(
        bincode::deserialize::<PipelineTypeWire>(&leftover).is_err(),
        "string leftover layout must fail closed"
    );
}
