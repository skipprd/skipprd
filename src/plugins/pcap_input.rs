use pcap::{Capture, Device, Packet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, OUTPUT_FILES_STATIC};

use crate::helpers::configuration::{Config};

use std::io::{BufRead, BufReader};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Instant;
use tokio::time::Duration;
use tracing::error;
use crate::helpers::Helpers;


pub struct DataSourcePcapPlugin {
    ingest: Ingest,
    buffer_size: usize,
    buffer_timeout: Duration,
    buffer_threshold: Duration,
}


impl DataSourcePcapPlugin {
    pub async fn new() -> DataSourcePcapPlugin {
        DataSourcePcapPlugin {
            ingest: Ingest::new(),
            buffer_size: Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1")
                .parse()
                .unwrap(),
            buffer_timeout: Duration::from_secs(
                Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "1")
                    .parse()
                    .unwrap(),
            ),
            buffer_threshold: Duration::from_secs(
                Config::getenv("BUFFER_THRESHOLD_SECONDS", "5")
                    .parse()
                    .unwrap(),
            ),
        }
    }

    pub async fn sync(&mut self, offsets: Arc<Offsets>) {
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();

        let interface_name = Config::getenv("DATA_SOURCE_INTERFACE_NAME", "en0");
        let buffer_size = self.buffer_size;
        let buffer_timeout = self.buffer_timeout;

        thread::spawn(move || {
            let mut cap = Capture::from_device(interface_name.as_str()).unwrap()
                .timeout(120000) // timeout in ms
                .open().unwrap();

            let mut buffer = Vec::new();
            let mut last_flush = Instant::now();

            while let Ok(packet) = cap.next_packet() {
                let json_data = packet_to_json(&packet);
                buffer.extend(json_data.into_bytes());

                if buffer.len() >= buffer_size || last_flush.elapsed() >= buffer_timeout {
                    if let Err(e) = tx.send(buffer.clone()) {
                        error!("Error sending to buffer channel: {}", e);
                        break;
                    }
                    buffer.clear();
                    last_flush = Instant::now();
                }
            }

            // Send any remaining data
            if !buffer.is_empty() {
                if let Err(e) = tx.send(buffer) {
                    error!("Error sending to buffer channel: {}", e);
                }
            }
        });

        loop {
            match rx.recv_timeout(self.buffer_threshold) {
                Ok(buffer) => {
                    let data = String::from_utf8_lossy(&buffer).to_string();
                    let batch = IngestBatch {
                        offset_key: OffsetKey {
                            namespace: "stdin".to_string(),
                            partition: Helpers::random_str(10),
                        },
                        data: data.clone(),
                        bytes: data.len(),
                        source_uri: "".to_string(),
                        namespace: None,
                    };

                    // Spawn a new task in the runtime for each batch received.
                    let offsets_clone = offsets.clone();

                    // thread::spawn(move || {
                    self.ingest.ingest_file(vec![batch], &offsets_clone)
                    // })
                    // .join()
                    // .unwrap();
                }
                Err(e) => match e {
                    mpsc::RecvTimeoutError::Timeout => {
                        let mut output_files = OUTPUT_FILES_STATIC.write().unwrap();
                        Ingest::flush_buffers(true, &mut output_files);
                    }
                    mpsc::RecvTimeoutError::Disconnected => {
                        error!("Error receiving from buffer channel: {}", e);
                        // break;
                    }
                },
            }
        }
    }
}



fn packet_to_json(packet: &Packet) -> String {
    let header = &packet.header;

    let ts_sec = header.ts.tv_sec;
    let ts_usec = header.ts.tv_usec;
    let capture_len = header.caplen;
    let packet_len = header.len;

    // Extracting more details from packet data will require parsing the packet data.
    // This can be complex, especially if you're dealing with various protocols.
    // For now, we'll demonstrate parsing the Ethernet, IP, and TCP/UDP headers.

    let ethertype = ((packet.data[12] as u16) << 8) | (packet.data[13] as u16);
    let mut payload_offset = 14; // Ethernet header size

    let mut ip_version = "".to_string();
    let mut src_ip = "".to_string();
    let mut dest_ip = "".to_string();
    let mut src_port = 0;
    let mut dest_port = 0;
    let mut protocol = "".to_string();

    // Handle IPv4 (0x0800) and IPv6 (0x86DD)
    match ethertype {
        0x0800 => {
            ip_version = "IPv4".to_string();
            src_ip = format!(
                "{}.{}.{}.{}",
                packet.data[payload_offset + 12],
                packet.data[payload_offset + 13],
                packet.data[payload_offset + 14],
                packet.data[payload_offset + 15]
            );

            dest_ip = format!(
                "{}.{}.{}.{}",
                packet.data[payload_offset + 16],
                packet.data[payload_offset + 17],
                packet.data[payload_offset + 18],
                packet.data[payload_offset + 19]
            );

            protocol = match packet.data[payload_offset + 9] {
                6 => "TCP",
                17 => "UDP",
                _ => "OTHER",
            }
                .to_string();

            payload_offset += (packet.data[payload_offset] & 0xF) as usize * 4; // IP header size
        }
        0x86DD => {
            ip_version = "IPv6".to_string();

            src_ip = Ipv6Addr::new(
                ((packet.data[payload_offset] as u16) << 8) | (packet.data[payload_offset + 1] as u16),
                ((packet.data[payload_offset + 2] as u16) << 8) | (packet.data[payload_offset + 3] as u16),
                ((packet.data[payload_offset + 4] as u16) << 8) | (packet.data[payload_offset + 5] as u16),
                ((packet.data[payload_offset + 6] as u16) << 8) | (packet.data[payload_offset + 7] as u16),
                ((packet.data[payload_offset + 8] as u16) << 8) | (packet.data[payload_offset + 9] as u16),
                ((packet.data[payload_offset + 10] as u16) << 8) | (packet.data[payload_offset + 11] as u16),
                ((packet.data[payload_offset + 12] as u16) << 8) | (packet.data[payload_offset + 13] as u16),
                ((packet.data[payload_offset + 14] as u16) << 8) | (packet.data[payload_offset + 15] as u16),
            ).to_string();

            dest_ip = Ipv6Addr::new(
                ((packet.data[payload_offset + 16] as u16) << 8) | (packet.data[payload_offset + 17] as u16),
                ((packet.data[payload_offset + 18] as u16) << 8) | (packet.data[payload_offset + 19] as u16),
                ((packet.data[payload_offset + 20] as u16) << 8) | (packet.data[payload_offset + 21] as u16),
                ((packet.data[payload_offset + 22] as u16) << 8) | (packet.data[payload_offset + 23] as u16),
                ((packet.data[payload_offset + 24] as u16) << 8) | (packet.data[payload_offset + 25] as u16),
                ((packet.data[payload_offset + 26] as u16) << 8) | (packet.data[payload_offset + 27] as u16),
                ((packet.data[payload_offset + 28] as u16) << 8) | (packet.data[payload_offset + 29] as u16),
                ((packet.data[payload_offset + 30] as u16) << 8) | (packet.data[payload_offset + 31] as u16),
            ).to_string();


            protocol = match packet.data[payload_offset + 6] {
                6 => "TCP",
                17 => "UDP",
                _ => "OTHER",
            }
                .to_string();

            payload_offset += 40; // IPv6 header size
        }
        _ => {}
    }

    // TCP or UDP port extraction, based on protocol
    if protocol == "TCP" || protocol == "UDP" {
        src_port = ((packet.data[payload_offset] as u16) << 8) | packet.data[payload_offset + 1] as u16;
        dest_port = ((packet.data[payload_offset + 2] as u16) << 8)
            | packet.data[payload_offset + 3] as u16;
    }

    // Construct the JSON string
    format!(
        r#"{{"timestamp_sec": {}, "timestamp_usec": {}, "capture_len": {}, "packet_len": {}, "ip_version": "{}", "src_ip": "{}", "dest_ip": "{}", "protocol": "{}", "src_port": {}, "dest_port": {}}}"#,
        ts_sec, ts_usec, capture_len, packet_len, ip_version, src_ip, dest_ip, protocol, src_port, dest_port
    )
}
