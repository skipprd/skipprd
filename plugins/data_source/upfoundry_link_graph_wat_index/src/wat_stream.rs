use std::io::ErrorKind;
use std::path::Path;
use std::pin::Pin;

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use flate2::{Decompress, FlushDecompress, Status};
use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::io::StreamReader;

const READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum WatStreamOpenError {
    TooLarge {
        compressed_bytes: u64,
        max_wat_object_bytes: usize,
    },
    Io(std::io::Error),
}

impl From<std::io::Error> for WatStreamOpenError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl std::fmt::Display for WatStreamOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge {
                compressed_bytes,
                max_wat_object_bytes,
            } => write!(
                f,
                "wat object is {compressed_bytes} bytes, above max_wat_object_bytes {max_wat_object_bytes}"
            ),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

async fn s3_client() -> S3Client {
    let cfg = aws_config::load_defaults(BehaviorVersion::latest()).await;
    S3Client::new(&cfg)
}

fn reject_if_too_large(
    compressed_bytes: Option<u64>,
    max_wat_object_bytes: usize,
) -> Result<(), WatStreamOpenError> {
    if let Some(bytes) = compressed_bytes {
        if bytes > max_wat_object_bytes as u64 {
            return Err(WatStreamOpenError::TooLarge {
                compressed_bytes: bytes,
                max_wat_object_bytes,
            });
        }
    }
    Ok(())
}

pub async fn open_wat_stream(
    path: &str,
    max_wat_object_bytes: usize,
) -> Result<WatGzipStream<Pin<Box<dyn AsyncRead + Send + Unpin>>>, WatStreamOpenError> {
    if Path::new(path).exists() {
        let file = tokio::fs::File::open(path).await?;
        let len = file.metadata().await?.len();
        reject_if_too_large(Some(len), max_wat_object_bytes)?;
        return Ok(WatGzipStream::new(Box::pin(file), max_wat_object_bytes));
    }

    if let Some(rest) = path.strip_prefix("s3://") {
        let (bucket, key) = rest.split_once('/').ok_or_else(|| {
            WatStreamOpenError::Io(std::io::Error::new(
                ErrorKind::InvalidInput,
                "invalid s3 uri",
            ))
        })?;
        let client = s3_client().await;
        let head = client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|err| WatStreamOpenError::Io(std::io::Error::other(err.to_string())))?;
        reject_if_too_large(head.content_length().map(|n| n as u64), max_wat_object_bytes)?;

        let output = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|err| WatStreamOpenError::Io(std::io::Error::other(err.to_string())))?;
        let reader = output
            .body
            .into_async_read();
        return Ok(WatGzipStream::new(Box::pin(reader), max_wat_object_bytes));
    }

    let url = if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("https://data.commoncrawl.org/{path}")
    };
    let client = reqwest::Client::new();
    let head = client
        .head(&url)
        .send()
        .await
        .map_err(|err| WatStreamOpenError::Io(std::io::Error::other(err.to_string())))?;
    reject_if_too_large(head.content_length(), max_wat_object_bytes)?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| WatStreamOpenError::Io(std::io::Error::other(err.to_string())))?;
    let stream = response
        .bytes_stream()
        .map_err(|err| std::io::Error::other(err.to_string()));
    let reader = StreamReader::new(stream);
    Ok(WatGzipStream::new(Box::pin(reader), max_wat_object_bytes))
}

pub struct WatGzipStream<R: AsyncRead + Unpin> {
    reader: R,
    buffer: Vec<u8>,
    stream_bytes: u64,
    eof: bool,
    max_object_bytes: usize,
}

impl<R: AsyncRead + Unpin> WatGzipStream<R> {
    pub fn new(reader: R, max_object_bytes: usize) -> Self {
        Self {
            reader,
            buffer: Vec::with_capacity(READ_CHUNK_BYTES),
            stream_bytes: 0,
            eof: false,
            max_object_bytes,
        }
    }

    pub fn bytes_read(&self) -> u64 {
        self.stream_bytes
    }

    fn buffer_base_offset(&self) -> u64 {
        self.stream_bytes.saturating_sub(self.buffer.len() as u64)
    }

    async fn fill_buffer(&mut self) -> Result<(), std::io::Error> {
        if self.eof {
            return Ok(());
        }
        let mut chunk = vec![0u8; READ_CHUNK_BYTES];
        let read = self.reader.read(&mut chunk).await?;
        if read == 0 {
            self.eof = true;
            return Ok(());
        }
        self.stream_bytes = self.stream_bytes.saturating_add(read as u64);
        if self.stream_bytes > self.max_object_bytes as u64 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "wat object exceeded max_wat_object_bytes {} while streaming",
                    self.max_object_bytes
                ),
            ));
        }
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(())
    }

    fn gzip_member_payload(
        compressed: &[u8],
        offset: usize,
    ) -> Result<(usize, Vec<u8>), std::io::Error> {
        if compressed.get(offset..offset + 10).is_none()
            || compressed[offset] != 0x1f
            || compressed[offset + 1] != 0x8b
            || compressed[offset + 2] != 8
        {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "not a gzip member",
            ));
        }
        let flags = compressed[offset + 3];
        let mut pos = offset + 10;
        if flags & 0x04 != 0 {
            let extra_len = u16::from_le_bytes([
                *compressed.get(pos).ok_or_else(|| {
                    std::io::Error::new(ErrorKind::UnexpectedEof, "gzip extra len")
                })?,
                *compressed.get(pos + 1).ok_or_else(|| {
                    std::io::Error::new(ErrorKind::UnexpectedEof, "gzip extra len")
                })?,
            ]) as usize;
            pos = pos.saturating_add(2).saturating_add(extra_len);
        }
        if flags & 0x08 != 0 {
            while *compressed.get(pos).ok_or_else(|| {
                std::io::Error::new(ErrorKind::UnexpectedEof, "gzip file name")
            })? != 0
            {
                pos += 1;
            }
            pos += 1;
        }
        if flags & 0x10 != 0 {
            while *compressed.get(pos).ok_or_else(|| {
                std::io::Error::new(ErrorKind::UnexpectedEof, "gzip comment")
            })? != 0
            {
                pos += 1;
            }
            pos += 1;
        }
        if flags & 0x02 != 0 {
            pos += 2;
        }
        if pos >= compressed.len() {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "gzip payload",
            ));
        }

        let mut payload = Vec::new();
        let mut decoder = Decompress::new(false);
        let mut out = [0u8; 8192];
        loop {
            let before_in = decoder.total_in();
            let before_out = decoder.total_out();
            let input_start = pos.saturating_add(usize::try_from(before_in).unwrap_or(0));
            let status = decoder
                .decompress(&compressed[input_start..], &mut out, FlushDecompress::None)
                .map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())
                })?;
            let written = usize::try_from(decoder.total_out().saturating_sub(before_out))
                .unwrap_or(0);
            payload.extend_from_slice(&out[..written]);
            if status == Status::StreamEnd {
                break;
            }
            let read = decoder.total_in().saturating_sub(before_in);
            if read == 0 && written == 0 {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "gzip deflate stream made no progress",
                ));
            }
        }
        let deflate_len = usize::try_from(decoder.total_in()).unwrap_or(0);
        let member_len = pos
            .saturating_sub(offset)
            .saturating_add(deflate_len)
            .saturating_add(8);
        if offset.saturating_add(member_len) > compressed.len() {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "gzip trailer",
            ));
        }
        Ok((member_len, payload))
    }

    fn member_needs_more_input(err: &std::io::Error) -> bool {
        matches!(err.kind(), ErrorKind::UnexpectedEof)
    }

    /// Returns the compressed byte range and decompressed member payload.
    pub async fn next_member(
        &mut self,
    ) -> Result<Option<(u64, u64, Vec<u8>)>, std::io::Error> {
        loop {
            let mut scan = 0usize;
            while scan < self.buffer.len() {
                if self.buffer[scan] != 0x1f {
                    scan += 1;
                    continue;
                }
                if scan + 2 >= self.buffer.len() {
                    break;
                }
                if self.buffer.get(scan + 1) != Some(&0x8b) {
                    scan += 1;
                    continue;
                }

                match Self::gzip_member_payload(&self.buffer, scan) {
                    Ok((consumed, payload)) => {
                        if consumed == 0 {
                            return Ok(None);
                        }
                        let abs_offset = self.buffer_base_offset().saturating_add(scan as u64);
                        self.buffer.drain(..scan.saturating_add(consumed));
                        return Ok(Some((
                            abs_offset,
                            u64::try_from(consumed).unwrap_or(0),
                            payload,
                        )));
                    }
                    Err(err) if Self::member_needs_more_input(&err) => break,
                    Err(_) => {
                        scan += 1;
                    }
                }
            }

            if scan > 0 && scan < self.buffer.len() {
                self.buffer.drain(..scan);
            }

            if self.eof {
                return Ok(None);
            }
            self.fill_buffer().await?;
            if self.eof && self.buffer.is_empty() {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[tokio::test]
    async fn streams_concatenated_members_without_loading_entire_object() {
        fn member(payload: &str) -> Vec<u8> {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(payload.as_bytes()).unwrap();
            encoder.finish().unwrap()
        }
        let first = member("WARC/1.0\r\n\r\n{\"ignored\":true}");
        let second_json = r#"{
            "Envelope": {
                "WARC-Header-Metadata": {
                    "WARC-Target-URI": "https://source.example/page"
                }
            }
        }"#;
        let second_payload = format!("WARC/1.0\r\n\r\n{second_json}");
        let second = member(&second_payload);
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second);

        let mut stream = WatGzipStream::new(bytes.as_slice(), usize::MAX);
        let first_member = stream.next_member().await.unwrap().unwrap();
        let second_member = stream.next_member().await.unwrap().unwrap();
        assert_eq!(first_member.0, 0);
        assert_eq!(first_member.1, first.len() as u64);
        assert_eq!(second_member.0, first.len() as u64);
        assert_eq!(second_member.1, second.len() as u64);
        assert!(stream.next_member().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stops_after_requested_member_count_without_reading_remainder() {
        fn member(payload: &str) -> Vec<u8> {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(payload.as_bytes()).unwrap();
            encoder.finish().unwrap()
        }
        let first = member("one");
        let second = member("two");
        let mut bytes = first;
        bytes.extend_from_slice(&second);

        let mut stream = WatGzipStream::new(bytes.as_slice(), usize::MAX);
        assert!(stream.next_member().await.unwrap().is_some());
        // Dropping here should not require reading the second member.
        drop(stream);
    }
}
