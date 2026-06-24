use sha2::{Digest, Sha256};

pub type Id64 = u64;
pub type Id128 = [u8; 16];

fn hash64(input: &str) -> Id64 {
    let digest = Sha256::digest(input.as_bytes());
    u64::from_be_bytes(digest[0..8].try_into().expect("8 bytes"))
}

fn hash128(input: &str) -> Id128 {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[0..16]);
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn url_id(canonical_url: &str) -> Id64 {
    hash64(&format!("url:{canonical_url}"))
}

pub fn domain_id(host: &str) -> Id64 {
    let normalized = host.trim().to_ascii_lowercase();
    hash64(&format!("domain:{normalized}"))
}

pub fn anchor_id(normalized_anchor: &str) -> Id64 {
    hash64(&format!("anchor:{normalized_anchor}"))
}

pub fn warc_file_id(warc_filename: &str) -> Id64 {
    hash64(&format!("warc:{warc_filename}"))
}

pub fn edge_id(
    source_url: &str,
    target_url: &str,
    rel_semantics: &str,
    link_context: &str,
) -> Id128 {
    hash128(&format!(
        "edge:{source_url}|{target_url}|{rel_semantics}|{link_context}"
    ))
}

pub fn edge_observation_id(
    edge: &Id128,
    cc_crawl_id: &str,
    warc_record_id: &str,
    link_ordinal: u32,
) -> Id128 {
    let edge_hex = hex_encode(edge);
    hash128(&format!(
        "obs:{edge_hex}|{cc_crawl_id}|{warc_record_id}|{link_ordinal}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable() {
        let u = url_id("https://skippr.io/");
        let d = domain_id("skippr.io");
        assert_ne!(u, 0);
        assert_ne!(d, 0);
        assert_eq!(url_id("https://skippr.io/"), u);
    }
}
