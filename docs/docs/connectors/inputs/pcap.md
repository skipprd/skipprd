# PCAP Input

Reads network packets from a PCAP capture source (file or interface).

> **Feature-gated:** this plugin requires the `pcap` compile-time feature. When built without the feature, the plugin returns an error at runtime.

## How it works

1. Opens a PCAP capture source (live interface or `.pcap` file).
2. Parses each packet and serializes it as a JSON record.
3. Batches are ingested through the standard WAL pipeline.

## Configuration

```yaml
data_sources:
  source:
    Pcap: {}
```

## Notes

- This plugin is experimental and requires the optional `pcap` feature flag at compile time.
- If the feature is not enabled, the plugin will return an error: `"pcap support not compiled -- enable the pcap feature"`.
