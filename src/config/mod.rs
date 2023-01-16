use std::collections::HashMap;

pub mod agent;
pub mod sidecar;
pub mod loader;

pub trait Config {
    type ServiceConfig: PartialEq + Eq;
    fn into_services(self) -> HashMap<String, Self::ServiceConfig>;
}