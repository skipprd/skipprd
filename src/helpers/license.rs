extern crate reqwest;
extern crate serde_json;

use crate::helpers::configuration::Config;
use once_cell::sync::Lazy;
use reqwest::{header::HeaderName, Client, Url};
use serde_derive::{Deserialize, Serialize};

use std::error::Error;
use std::string::ToString;

use std::sync::{RwLock};
use crate::helpers::timed_rwlock::TimedRwLock;

pub static TENANT_ID: Lazy<TimedRwLock<String>> = Lazy::new(|| TimedRwLock::new("tenant_id".to_string(), "".to_string()));
pub static HAS_LICENSE: Lazy<TimedRwLock<bool>> = Lazy::new(|| TimedRwLock::new("has_license".to_string(), false));

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
        let api_key = Config::get_skippr_api_token();
        Ok(Self {
            client: Client::new(),
            license_is_valid: false,
            license: None,
            api_key,
        })
    }

    pub async fn get_license(&mut self) -> Result<(), Box<dyn Error>> {
        let env = Config::get_pipeline_env();
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
            // println!("{:?}", response.json::<HashMap<String, String>>().await?);
        }

        match self.license_is_valid {
            true => {
                // println!("Found valid license for API key");
                println!("By using this software, you agree to the terms of the End User License Agreement (EULA) available at https://skippr.io/terms/eula");
                // println!("");
                Ok(())
            }
            _false => {
                // ASCI art generated from http://patorjk.com/software/taag/#p=display&f=ANSI%20Shadow&t=Skippr

                println!("Free local developer version. Visit https://skippr.io/pricing for additional plugins, schema evolution and metadata API.");
                println!("");
                println!("By using this software, you agree to the terms of the End User License Agreement (EULA) available at https://skippr.io/terms/eula");
                println!("");


                Ok(())
            }
        }
    }
}
