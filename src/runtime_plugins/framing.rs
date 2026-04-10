use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::runtime_plugins::protocol::{HostFrame, PluginFrame};

async fn read_len_prefixed<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let frame_len = reader.read_u32_le().await? as usize;
    let mut bytes = vec![0u8; frame_len];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_len_prefixed<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

pub async fn read_host_frame<R>(reader: &mut R) -> io::Result<HostFrame>
where
    R: AsyncRead + Unpin,
{
    let bytes = read_len_prefixed(reader).await?;
    bincode::deserialize(&bytes).map_err(|err| io::Error::other(err.to_string()))
}

pub async fn write_host_frame<W>(writer: &mut W, frame: &HostFrame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = bincode::serialize(frame).map_err(|err| io::Error::other(err.to_string()))?;
    write_len_prefixed(writer, &bytes).await
}

pub async fn read_plugin_frame<R>(reader: &mut R) -> io::Result<PluginFrame>
where
    R: AsyncRead + Unpin,
{
    let bytes = read_len_prefixed(reader).await?;
    bincode::deserialize(&bytes).map_err(|err| io::Error::other(err.to_string()))
}

pub async fn write_plugin_frame<W>(writer: &mut W, frame: &PluginFrame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = bincode::serialize(frame).map_err(|err| io::Error::other(err.to_string()))?;
    write_len_prefixed(writer, &bytes).await
}
