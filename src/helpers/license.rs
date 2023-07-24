extern crate reqwest;
extern crate serde_json;

use crate::helpers::configuration::Config;
use once_cell::sync::Lazy;
use reqwest::{header::HeaderName, Client, Url};
use serde_derive::{Deserialize, Serialize};

use std::error::Error;
use std::process::exit;
use std::sync::{Mutex, RwLock};

pub static TENANT_ID: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("".to_string()));
pub static HAS_LICENSE: Lazy<RwLock<bool>> = Lazy::new(|| RwLock::new(false));

const API_KEY_ENV_VAR: &str = "SKIPPR_API_TOKEN";
const APP_ENV: &str = "APP_ENV";
const DEFAULT_ENV: &str = "prod";

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
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let api_key = Config::getenv(API_KEY_ENV_VAR, "");
        Ok(Self {
            client: Client::new(),
            license_is_valid: false,
            license: None,
            api_key,
        })
    }

    pub async fn get_license(&mut self) -> Result<(), Box<dyn Error>> {
        let env = Config::getenv(APP_ENV, DEFAULT_ENV);
        let base_url = if env != DEFAULT_ENV {
            format!("https://license.{}.api.skippr.io", env)
        } else {
            String::from("https://license.api.skippr.io")
        };

        let _url = Url::parse(&base_url)?.join(&format!("license-api/check/{}", self.api_key))?;

        let auth_header = HeaderName::from_static("x-api-key");

        let req = self
            .client
            .get(base_url)
            .header(auth_header, self.api_key.clone());

        let response = req.send().await?;

        if response.status().is_success() {
            let body = response.json::<LicenseRecord>().await?;
            self.license = Some(body);
            // println!("{:?}", self.license);

            TENANT_ID
                .write()
                .unwrap()
                .push_str(&self.license.as_ref().unwrap().tenant);

            self.license_is_valid = self
                .license
                .as_ref()
                .map_or(false, |l| &self.api_key == &l.api_key);

            let mut has_license = match HAS_LICENSE.write() {
                Ok(mut val) => {
                    val
                },
                Err(err) => {
                    println!("Error: {:?}", err);
                    std::process::exit(1)
                }
            };

            *has_license = self.license_is_valid;

        } else {
            self.license_is_valid = false;
            // println!("{:?}", response.json::<HashMap<String, String>>().await?);
        }

        match self.license_is_valid {
            true => {
                println!("Found valid license for API key");
                Ok(())
            }
            _false => {
                // Free local developer version, visit https://skippr.io to get a license
                println!("Free local developer version. Visit https://skippr.io/upgrade to upgrade pluigns and metadata api at anytime");
                Ok(())
            }
        }
    }
}
