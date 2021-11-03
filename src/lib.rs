pub mod config;

pub const OPEN_STREAM_COMMAND: &'static str = "open";
pub const CLOSE_STREAM_COMMAND: &'static str = "close";

pub enum TunnelCommand {
    Open,
    Close,
    Tx(Vec<u8>),
    ClientDisconnected,
}
