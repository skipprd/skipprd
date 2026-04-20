use std::io::{self, ErrorKind};

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const MAX_RUNTIME_FRAME_BYTES: usize = 256 * 1024 * 1024;

async fn read_len_prefixed<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let frame_len = reader.read_u32_le().await? as usize;
    if frame_len > MAX_RUNTIME_FRAME_BYTES {
        return Err(io::Error::other(format!(
            "runtime frame length {} exceeds limit {}",
            frame_len, MAX_RUNTIME_FRAME_BYTES
        )));
    }
    let mut bytes = vec![0u8; frame_len];
    reader.read_exact(&mut bytes).await?;
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
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => Ok(None),
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
