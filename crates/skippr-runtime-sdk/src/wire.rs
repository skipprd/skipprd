use std::io::{self, ErrorKind};

use bincode::Options;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_RUNTIME_FRAME_BYTES: usize = 256 * 1024 * 1024;
const CLEAN_RUNTIME_EOF: &str = "runtime channel closed before next frame";
/// HostFrame::PrepareSink is variant 6. A 6-byte payload whose first u32 is a
/// request_id means the plugin treated that tag as the next length prefix.
const PREPARE_SINK_VARIANT_TAG: u32 = 6;

pub(crate) fn looks_like_omitted_prepare_sink_length_prefix(bytes: &[u8]) -> bool {
    // PluginFrame::Installed is a 4-byte unit variant (`[01,00,00,00]`). Warning
    // on every payload under 32 bytes treated that handshake ack as the EU
    // Staging COPY desync.
    bytes.len() == PREPARE_SINK_VARIANT_TAG as usize
}

pub fn decode_payload<T>(bytes: &[u8]) -> io::Result<T>
where
    T: DeserializeOwned,
{
    if looks_like_omitted_prepare_sink_length_prefix(bytes) {
        tracing::warn!(
            "runtime ipc short frame payload_len={} prefix={:02x?} (PrepareSink tag={}; short payloads usually mean the previous length prefix was skipped)",
            bytes.len(),
            &bytes[..bytes.len().min(16)],
            PREPARE_SINK_VARIANT_TAG
        );
    }
    bincode::options()
        .with_fixint_encoding()
        .with_limit(MAX_RUNTIME_FRAME_BYTES as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)
        .map_err(|err| {
            let prefix = bytes.len().min(16);
            io::Error::other(format!(
                "{err}; payload_len={} prefix={:02x?}",
                bytes.len(),
                &bytes[..prefix]
            ))
        })
}

async fn read_len_prefixed<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    let mut prefix_read = 0usize;
    while prefix_read < prefix.len() {
        match reader.read(&mut prefix[prefix_read..]).await? {
            0 if prefix_read == 0 => {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, CLEAN_RUNTIME_EOF));
            }
            0 => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "runtime frame truncated while reading length prefix",
                ));
            }
            read => prefix_read += read,
        }
    }

    let frame_len = u32::from_le_bytes(prefix) as usize;
    if frame_len > MAX_RUNTIME_FRAME_BYTES {
        return Err(io::Error::other(format!(
            "runtime frame length {} exceeds limit {}",
            frame_len, MAX_RUNTIME_FRAME_BYTES
        )));
    }
    let mut bytes = vec![0u8; frame_len];
    let mut payload_read = 0usize;
    while payload_read < frame_len {
        match reader.read(&mut bytes[payload_read..]).await? {
            0 => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "runtime frame truncated while reading payload (expected {} bytes, read {})",
                        frame_len, payload_read
                    ),
                ));
            }
            read => payload_read += read,
        }
    }
    Ok(bytes)
}

async fn write_len_prefixed<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if bytes.len() > MAX_RUNTIME_FRAME_BYTES {
        return Err(io::Error::other(format!(
            "runtime frame length {} exceeds limit {}",
            bytes.len(),
            MAX_RUNTIME_FRAME_BYTES
        )));
    }
    // Small frames (control, handshake, PrepareSink) go out as one write so a
    // retry cannot leave a bare 4-byte variant tag as the next length prefix.
    // Large data chunks stay two writes under the caller's mutex.
    if bytes.len() <= 4 * 1024 * 1024 {
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(bytes);
        writer.write_all(&framed).await?;
    } else {
        writer.write_u32_le(bytes.len() as u32).await?;
        writer.write_all(bytes).await?;
    }
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let bytes = read_len_prefixed(reader).await?;
    tracing::debug!(
        "runtime ipc read payload_len={} prefix={:02x?}",
        bytes.len(),
        &bytes[..bytes.len().min(16)]
    );
    decode_payload(&bytes)
}

pub async fn read_frame_or_eof<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    match read_frame(reader).await {
        Ok(frame) => Ok(Some(frame)),
        Err(err)
            if err.kind() == ErrorKind::UnexpectedEof && err.to_string() == CLEAN_RUNTIME_EOF =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

pub async fn write_frame<W, T>(writer: &mut W, frame: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = bincode::serialize(frame).map_err(|err| io::Error::other(err.to_string()))?;
    write_len_prefixed(writer, &bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HostFrame, PluginFrame};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
    struct TestFrame {
        value: u32,
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_warns(f: impl FnOnce()) -> String {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().expect("capture").clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn installed_plugin_frame_is_four_bytes_not_prepare_sink_desync() {
        let payload = bincode::serialize(&PluginFrame::Installed).unwrap();
        assert_eq!(payload.as_slice(), &[1, 0, 0, 0]);
        assert!(!looks_like_omitted_prepare_sink_length_prefix(&payload));
        let decoded: PluginFrame = decode_payload(&payload).unwrap();
        assert!(matches!(decoded, PluginFrame::Installed));
    }

    #[test]
    fn installed_plugin_frame_does_not_warn_as_short_frame() {
        let logged = capture_warns(|| {
            let payload = bincode::serialize(&PluginFrame::Installed).unwrap();
            let _: PluginFrame = decode_payload(&payload).unwrap();
        });
        assert!(
            !logged.contains("short frame") && !logged.contains("payload_len=4"),
            "{logged}"
        );
    }

    #[test]
    fn host_shutdown_unit_variant_is_four_bytes_not_desync() {
        let payload = bincode::serialize(&HostFrame::Shutdown).unwrap();
        assert_eq!(payload.len(), 4);
        assert!(!looks_like_omitted_prepare_sink_length_prefix(&payload));
    }

    #[test]
    fn omitted_prepare_sink_length_warns_on_six_byte_eu_staging_payload() {
        let eu = [0x1f, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(looks_like_omitted_prepare_sink_length_prefix(&eu));
        let logged = capture_warns(|| {
            let _ = decode_payload::<HostFrame>(&eu);
        });
        assert!(
            logged.contains("short frame")
                && logged.contains("payload_len=6")
                && logged.contains("1f"),
            "{logged}"
        );
    }

    #[tokio::test]
    async fn read_frame_or_eof_treats_clean_close_as_none() {
        let (writer, mut reader) = tokio::io::duplex(64);
        drop(writer);

        let frame = read_frame_or_eof::<_, TestFrame>(&mut reader)
            .await
            .unwrap();
        assert_eq!(frame, None);
    }

    #[tokio::test]
    async fn read_frame_or_eof_rejects_truncated_length_prefix() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[1, 2]).await.unwrap();
        drop(writer);

        let err = read_frame_or_eof::<_, TestFrame>(&mut reader)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert!(err
            .to_string()
            .contains("runtime frame truncated while reading length prefix"));
    }

    #[tokio::test]
    async fn read_frame_or_eof_rejects_truncated_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&4u32.to_le_bytes()).await.unwrap();
        writer.write_all(&[1, 2]).await.unwrap();
        drop(writer);

        let err = read_frame_or_eof::<_, TestFrame>(&mut reader)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert!(err
            .to_string()
            .contains("runtime frame truncated while reading payload"));
    }

    #[tokio::test]
    async fn read_frame_includes_payload_prefix_on_decode_error() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&0u32.to_le_bytes()).await.unwrap();
        drop(writer);

        let err = read_frame_or_eof::<_, TestFrame>(&mut reader)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("prefix=") && message.contains("[]"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn read_frame_rejects_trailing_bytes_that_would_desync_the_next_frame() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let payload = bincode::serialize(&TestFrame { value: 7 }).unwrap();
        let mut padded = payload.clone();
        padded.extend_from_slice(&PREPARE_SINK_VARIANT_TAG.to_le_bytes());
        writer
            .write_all(&(padded.len() as u32).to_le_bytes())
            .await
            .unwrap();
        writer.write_all(&padded).await.unwrap();
        drop(writer);

        let err = read_frame_or_eof::<_, TestFrame>(&mut reader)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("payload_len=") && message.contains("prefix="),
            "{message}"
        );
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_two_sequential_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        write_frame(&mut writer, &TestFrame { value: 1 })
            .await
            .unwrap();
        write_frame(&mut writer, &TestFrame { value: 2 })
            .await
            .unwrap();
        drop(writer);

        assert_eq!(
            read_frame::<_, TestFrame>(&mut reader).await.unwrap(),
            TestFrame { value: 1 }
        );
        assert_eq!(
            read_frame::<_, TestFrame>(&mut reader).await.unwrap(),
            TestFrame { value: 2 }
        );
    }
}
