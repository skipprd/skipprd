# OTLP fixtures

Hermetic protobuf/JSON Export*ServiceRequest samples used by plugin decode tests.

Generate protobuf bytes from the in-crate builders:

```bash
cargo test -p skippr-plugin-data-source-otlp decode_traces_fixture_counts -- --nocapture
```

JSON files follow the OTLP/JSON mapping (`resourceSpans`, camelCase, hex IDs). No PII.
