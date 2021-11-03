pub use config::ConfigError;
use serde_derive::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub client_tcp_addr: String,
    pub ws_url: String,
    pub server_tcp_port: u16,
    pub server_ws_port: u16,
    pub api_key: String,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut cfg = ::config::Config::new();
        cfg.merge(::config::Environment::new())?;
        cfg.try_into()
    }
}
