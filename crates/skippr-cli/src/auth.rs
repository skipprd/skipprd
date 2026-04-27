use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub access_token: String,
    pub refresh_token: String,
}

fn credentials_path() -> PathBuf {
    let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = home.join(".skippr");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("credentials.json")
}

pub fn save_credentials(creds: &StoredCredentials) {
    let path = credentials_path();
    let json = serde_json::to_string_pretty(creds).unwrap();
    std::fs::write(&path, json).unwrap_or_else(|e| {
        eprintln!("warning: failed to save credentials: {}", e);
    });
}

pub fn load_credentials() -> Option<StoredCredentials> {
    let path = credentials_path();
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn clear_credentials() {
    let path = credentials_path();
    let _ = std::fs::remove_file(&path);
}

pub fn auth_base_url() -> String {
    std::env::var("SKIPPR_AUTH_URL").unwrap_or_else(|_| "https://auth.skippr.io".to_string())
}
