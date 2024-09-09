extern crate reqwest;
extern crate serde_json;

use std::error::Error;
use crate::helpers::configuration::Config;
use once_cell::sync::Lazy;
use reqwest::{header::HeaderName, Client, Url};
use serde_derive::{Deserialize, Serialize};

use thiserror::Error;
use std::string::ToString;

use crate::helpers::timed_rwlock::TimedRwLock;

pub static TENANT_ID: Lazy<TimedRwLock<String>> = Lazy::new(|| TimedRwLock::new("tenant_id".to_string(), "".to_string()));
pub static HAS_LICENSE: Lazy<TimedRwLock<bool>> = Lazy::new(|| TimedRwLock::new("has_license".to_string(), false));

const DEFAULT_ENV: &str = "prod";

#[derive(Debug, Error)]
#[error("Found no license for the provided API key: {0}")]
struct NoLicenseError(String);

pub struct LicenseChecker {
    client: Client,
    pub license_is_valid: bool,
    pub license: Option<LicenseRecord>,
    api_key: String,
}

#[derive(Serialize, Deserialize)]
pub struct LicenseRecord {
    api_key: String,
    username: String,
    tenant: String,
    valid_from: String,
    valid_to: Option<String>,
}

impl LicenseChecker {
    pub fn new() -> Self {
        let api_key = Config::get_skippr_api_token();
        Self {
            client: Client::new(),
            license_is_valid: false,
            license: None,
            api_key,
        }
    }

    pub async fn get_license(&mut self) -> Result<(), Box<dyn Error>> {
        
        if HAS_LICENSE.read().clone() {
            self.license_is_valid = true;
            return Ok(());
        }
        
        self.license_is_valid = false;
        
        let env = Config::get_pipeline_env();
        let base_url = if env != DEFAULT_ENV {
            format!("https://license.{}.api.skippr.io", env)
        } else {
            String::from("https://license.api.skippr.io")
        };
        
        // let url = Url::parse(&base_url)?.join(&format!("license-api/check/{}", self.api_key))?;
        
        let auth_header = HeaderName::from_static("x-api-key");
        
        let req = self
            .client
            .get(base_url)
            .header(auth_header, self.api_key.clone());
        
        let response = req.send().await?;
        
        if response.status().is_success() {
            let body = response.json::<LicenseRecord>().await?;
            self.license = Some(body);
        
            TENANT_ID
                .write()
                .clear();
            TENANT_ID
                .write()
                .push_str(&self.license.as_ref().unwrap().tenant);
        
            self.license_is_valid = self
                .license
                .as_ref()
                .map_or(false, |l| &self.api_key == &l.api_key);
        
            let mut has_license = HAS_LICENSE.write();
        
            *has_license = self.license_is_valid;
            
        } else {
            self.license_is_valid = false;
        }
        
        match self.license_is_valid {
            true => {
                println!("By using this software, you agree to the terms of the End User License Agreement (EULA) available at https://skippr.io/terms/eula");
                Ok(())
            }
            _false => {
               Err(NoLicenseError(self.api_key.clone()).into())
            }
        }
    }
}
