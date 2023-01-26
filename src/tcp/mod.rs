use url::Url;

pub mod fwd;
pub mod service;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub host: String,
    pub port: u16,
}

struct Visitor;

impl<'de> serde::de::Visitor<'de> for Visitor {
    type Value = Address;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let url = Url::parse(v).map_err(|error| E::custom(error))?;
        if url.scheme() != "tcp" {
            Err(E::custom("expected 'tcp' scheme"))?;
        }
        let host = url
            .host()
            .ok_or_else(|| E::custom("missing host"))?
            .to_string();
        let port = url.port().ok_or_else(|| E::custom("missing port"))?;
        Ok(Self::Value { host, port })
    }
}

impl<'de> serde::de::Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(Visitor)
    }
}
