use std::io::{self, ErrorKind};

use bincode::Options;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const MAX_RUNTIME_FRAME_BYTES: usize = 256 * 1024 * 1024;
const CLEAN_RUNTIME_EOF: &str = "runtime channel closed before next frame";
/// HostFrame::PrepareSink is variant 6. A 6-byte payload whose first u32 is a
/// request_id means the plugin treated that tag as the next length prefix.
const PREPARE_SINK_VARIANT_TAG: u32 = 6;

pub(crate) fn decode_payload<T>(bytes: &[u8]) -> io::Result<T>
where
    T: DeserializeOwned,
{
    if bytes.len() < 32 {
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

    #[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
    struct TestFrame {
        value: u32,
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
