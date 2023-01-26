use std::{collections::HashMap, net::SocketAddr};

#[derive(serde::Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum ServiceConfig {
    #[serde(rename = "dns")]
    DNS {
        path: String,
        listen: SocketAddr,
        timeout: u64,
    },

    #[serde(rename = "tcp")]
    TCP { path: String, listen: SocketAddr },
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
