use std::collections::HashMap;

use crate::{dns::forwarder, tcp};

#[derive(serde::Deserialize, PartialEq, Eq, Clone)]
#[serde(tag = "type")]
pub enum ServiceConfig {
    #[serde(rename = "dns")]
    DNS {
        path: String,
        address: forwarder::Address,
        timeout: u64,
    },

    #[serde(rename = "tcp")]
    TCP { path: String, address: tcp::Address },
}

#[derive(serde::Deserialize)]
pub struct Config {
    #[serde(rename = "service")]
    pub services: HashMap<String, ServiceConfig>,
}

impl super::Config for Config {
    type ServiceConfig = ServiceConfig;
    fn into_services(self) -> HashMap<String, Self::ServiceConfig> {
        self.services
    }
}
