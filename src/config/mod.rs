use std::collections::HashMap;

pub mod agent;
pub mod loader;
pub mod sidecar;

pub trait Config {
    type ServiceConfig: PartialEq + Eq;
    fn into_services(self) -> HashMap<String, Self::ServiceConfig>;
}
