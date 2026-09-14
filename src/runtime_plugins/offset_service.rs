use std::io;
use std::sync::Arc;

use once_cell::sync::Lazy;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tracing::warn;

use crate::helpers::offsets::{OffsetTypes, Offsets};
use crate::runtime_plugins::protocol::{
    HostOffsetFrame, PluginOffsetFrame, RuntimeOffsetValidationEntry, RuntimeSessionHello,
    RUNTIME_PROTOCOL_VERSION,
};
use crate::runtime_plugins::wire::{read_frame, write_frame};

static OFFSET_SERVICE_RT: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("offset service runtime"));

fn should_process(
    result: Result<Option<bool>, crate::helpers::offsets::OffsetsError>,
    offset_type: OffsetTypes,
) -> bool {
    match result {
        Err(_) => false,
        Ok(None) => true,
        Ok(Some(allow)) => match offset_type {
            OffsetTypes::Closed => !allow,
            OffsetTypes::Filesize | OffsetTypes::Position => allow,
        },
    }
}

fn validate_entries(offsets: &Offsets, entries: &[RuntimeOffsetValidationEntry]) -> Vec<bool> {
    entries
        .iter()
        .map(|entry| {
            should_process(
                offsets.validate(&entry.key, entry.offset_type, entry.offset_value),
                entry.offset_type,
            )
        })
        .collect()
}

async fn handle_offset_connection(
    mut stream: TcpStream,
    offsets: Arc<Offsets>,
    expected_token: String,
) -> io::Result<()> {
    let hello: RuntimeSessionHello = read_frame(&mut stream).await?;
    if hello.protocol_version != RUNTIME_PROTOCOL_VERSION {
        return Err(io::Error::other(format!(
            "offset service protocol mismatch: host={} child={}",
            RUNTIME_PROTOCOL_VERSION, hello.protocol_version
        )));
    }
    if hello.token != expected_token {
        return Err(io::Error::other("offset service session token mismatch"));
    }

    loop {
        let frame: PluginOffsetFrame = match read_frame(&mut stream).await {
            Ok(frame) => frame,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };

        match frame {
            PluginOffsetFrame::ValidateOffsetBatch {
                request_id,
                entries,
            } => {
                let should_process = validate_entries(&offsets, &entries);
                write_frame(
                    &mut stream,
                    &HostOffsetFrame::ValidateOffsetBatchResponse {
                        request_id,
                        should_process,
                    },
                )
                .await?;
            }
            PluginOffsetFrame::LoadCheckpoint { request_id, key } => {
                let envelope = offsets
                    .load_checkpoint_envelope(&key)
                    .map_err(io::Error::other)?;
                write_frame(
                    &mut stream,
                    &HostOffsetFrame::LoadCheckpointResponse {
                        request_id,
                        envelope,
                    },
                )
                .await?;
            }
        }
    }
}

pub struct OffsetServiceEndpoint {
    pub addr: std::net::SocketAddr,
    pub session_token: String,
    _task: tokio::task::JoinHandle<()>,
}

impl OffsetServiceEndpoint {
    pub fn spawn(offsets: Arc<Offsets>, session_token: String) -> io::Result<Self> {
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let addr = std_listener.local_addr()?;
        std_listener.set_nonblocking(true)?;
        let listener = std::sync::Arc::new(TcpListener::from_std(std_listener)?);
        let token_for_accept = session_token.clone();
        let offsets_for_accept = offsets.clone();

        let task = OFFSET_SERVICE_RT.spawn(async move {
            loop {
                let accept = listener.accept().await;
                let Ok((stream, _)) = accept else {
                    continue;
                };
                let offsets = offsets_for_accept.clone();
                let token = token_for_accept.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_offset_connection(stream, offsets, token).await {
                        warn!("offset service connection failed: {err}");
                    }
                });
            }
        });

        Ok(Self {
            addr,
            session_token,
            _task: task,
        })
    }

    pub fn env_value(&self) -> String {
        self.addr.to_string()
    }

    pub fn session_token(&self) -> &str {
        &self.session_token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::offsets::OffsetKey;
    use serial_test::serial;

    #[test]
    #[serial]
    fn validate_entries_closed_semantics() {
        let offsets = match Offsets::init() {
            Ok(offsets) => Arc::new(offsets),
            Err(_) => return,
        };
        offsets.clear_for_test();

        let missing = vec![RuntimeOffsetValidationEntry {
            key: OffsetKey::new("ns", "missing-key"),
            offset_type: OffsetTypes::Closed,
            offset_value: 1,
        }];
        assert_eq!(validate_entries(&offsets, &missing), vec![true]);

        let closed_key = OffsetKey::new("ns", "closed-key");
        offsets.set(&closed_key, OffsetTypes::Closed, 1).unwrap();
        let closed = vec![RuntimeOffsetValidationEntry {
            key: closed_key,
            offset_type: OffsetTypes::Closed,
            offset_value: 1,
        }];
        assert_eq!(validate_entries(&offsets, &closed), vec![false]);
    }

    #[test]
    fn store_error_does_not_process() {
        let err = crate::helpers::offsets::OffsetsError::Store("dynamo down".into());
        assert!(!should_process(Err(err), OffsetTypes::Closed));
        assert!(!should_process(
            Err(crate::helpers::offsets::OffsetsError::Store("x".into())),
            OffsetTypes::Position
        ));
    }
}
