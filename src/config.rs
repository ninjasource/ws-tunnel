use serde_derive::Deserialize;
use std::fs;

#[derive(Deserialize, Clone)]
pub struct ServerConfig {
    pub ws_addr: String,
    pub ws_path: String,
    pub clients: Vec<Client>,
}

#[derive(Deserialize, Clone)]
pub struct Client {
    pub tcp_addr: String,
    pub username: String,
    pub password: String,
}

impl ServerConfig {
    pub fn from_file() -> Result<Self, ConfigError> {
        let s = fs::read_to_string("./server.conf")?;
        let cfg: ServerConfig = serde_json::from_str(&s)?;
        Ok(cfg)
    }
}

#[derive(Deserialize, Debug)]
pub struct ClientConfig {
    pub tcp_addr: String,
    pub ws_url: String,
    // pub ws_tls_port: u16,
    pub username: String,
    pub password: String,
}

impl ClientConfig {
    pub fn from_file() -> Result<Self, ConfigError> {
        let s = fs::read_to_string("./client.conf")?;
        let cfg: ClientConfig = serde_json::from_str(&s)?;
        Ok(cfg)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
