extern crate reqwest;
extern crate serde_json;

use crate::helpers::configuration::Config;
use reqwest::{header::HeaderName, Client, Response, Url};
use std::collections::HashMap;
use std::error::Error;
use std::process::exit;

const API_KEY_ENV_VAR: &str = "SKIPPR_API_TOKEN";
const APP_ENV: &str = "APP_ENV";
const DEFAULT_ENV: &str = "prod";

pub struct LicenseChecker {
    client: Client,
    pub license_is_valid: bool,
    pub license: Option<HashMap<String, String>>,
    api_key: String,
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

        let url = Url::parse(&base_url)?.join(&format!("license-api/check/{}", self.api_key))?;

        let auth_header = HeaderName::from_static("x-api-key");

        let req = self
            .client
            .get(base_url)
            .header(auth_header, self.api_key.clone());

        let response = req.send().await?;

        if response.status().is_success() {
            let body = response.json::<HashMap<String, String>>().await?;
            self.license = Some(body);
            // println!("{:?}", self.license);

            self.license_is_valid = self
                .license
                .as_ref()
                .map_or(false, |l| l.get("api_key") == Some(&self.api_key));
        } else {
            self.license_is_valid = false;
            // println!("{:?}", response.json::<HashMap<String, String>>().await?);
        }

        match self.license_is_valid {
            true => {
                println!("Found valid license");
                Ok(())
            }
            _false => {
                println!("No valid license found for API Key");
                exit(1);
            }
        }
    }
}
