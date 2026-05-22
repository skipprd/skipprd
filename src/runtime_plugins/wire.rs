use std::io::{self, ErrorKind};

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const MAX_RUNTIME_FRAME_BYTES: usize = 256 * 1024 * 1024;
const CLEAN_RUNTIME_EOF: &str = "runtime channel closed before next frame";

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
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let bytes = read_len_prefixed(reader).await?;
    bincode::deserialize(&bytes).map_err(|err| io::Error::other(err.to_string()))
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
}
