use dotenv::dotenv;
use embedded_websocket::{
    framer::{Framer, FramerError, ReadResult, Stream},
    WebSocketClient, WebSocketOptions, WebSocketSendMessageType,
};
use log::{debug, error, info};
//use rand::prelude::ThreadRng;
use std::{
    error::Error,
    io::ErrorKind,
    net::{Shutdown, TcpStream},
    sync::mpsc::{channel, RecvTimeoutError, Sender},
    thread,
    time::Duration,
};
use url::Url;
use ws_tunnel::{CLOSE_STREAM_COMMAND, OPEN_STREAM_COMMAND};

use std::sync::Arc;

use std::convert::TryInto;

use rustls::{self, ClientConnection, ServerName};
use webpki_roots;

use rustls::{OwnedTrustAnchor, RootCertStore};

struct TlsStream<'a> {
    tls: rustls::Stream<'a, ClientConnection, TcpStream>,
}

impl<'a> embedded_websocket::framer::Stream<std::io::Error> for TlsStream<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        std::io::Read::read(&mut self.tls, buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        std::io::Write::write_all(&mut self.tls, buf)
    }
}

enum EgresPayload {
    Ping,
    TunnelBytes(Vec<u8>),
}

fn main() -> Result<(), FramerError<impl Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // load config from .env file
    dotenv().ok();
    let config = ws_tunnel::config::Config::from_env().expect("Invalid config");
    let url = Url::parse(&config.ws_url).expect("Invalid client_ws_url");
    let host = url.host_str().expect("Invalid url host name");

    let address = match url.port() {
        Some(port) => format!("{}:{}", host, port),
        None => host.to_owned(),
    };

    info!("Connecting to: {}", address);

    let mut root_store = RootCertStore::empty();
    root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));
    let tls_config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let server_name: ServerName = address.as_str().try_into().unwrap();
    let mut connection = rustls::ClientConnection::new(Arc::new(tls_config), server_name).unwrap();
    let mut stream = TcpStream::connect(format!("{}:443", &address)).map_err(FramerError::Io)?;

    const TIMEOUT: Duration = Duration::from_millis(50);
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();

    let tls = rustls::Stream::new(&mut connection, &mut stream);
    let mut stream = TlsStream { tls };

    info!("Tcp connected");

    let mut read_buf = [0; 4000];
    let mut read_cursor = 0;
    let mut write_buf = [0; 4000];
    let mut frame_buf = [0; 4000];
    let mut websocket = WebSocketClient::new_client(rand::thread_rng());

    let auth = format!("Authorization: Bearer {}", config.api_key);
    let headers = [auth.as_str()];

    // initiate a websocket opening handshake
    let websocket_options = WebSocketOptions {
        path: url.path(),
        host,
        origin: host,
        sub_protocols: None,
        additional_headers: Some(&headers),
    };

    let mut framer = Framer::new(
        &mut read_buf,
        &mut read_cursor,
        &mut write_buf,
        &mut websocket,
    );

    loop {
        match framer.connect(&mut stream, &websocket_options) {
            Ok(_) => break, // do nothing
            Err(FramerError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                // a normal timeout
            }
            Err(e) => return Err(e),
        }
    }

    info!("Websocket connected");

    // egres is tcp data leaving this machine via websocket
    let (tx_egres, rx_egres) = channel::<EgresPayload>();
    let tx_egres_clone = tx_egres.clone();

    // heartbeat
    thread::spawn(move || loop {
        tx_egres_clone.send(EgresPayload::Ping).unwrap();
        thread::sleep(Duration::from_secs(20))
    });

    // receive bytes comming from websocket and push them to the internal stream
    let mut tunnel: Option<TcpStream> = None;
    loop {
        match framer.read(&mut stream, &mut frame_buf) {
            Ok(ReadResult::Text(command)) => {
                info!("Received command: {}", command);

                if command == OPEN_STREAM_COMMAND {
                    shutdown(&mut tunnel);
                    tunnel = Some(open_tunnel(&config.client_tcp_addr, tx_egres.clone()));
                } else if command == CLOSE_STREAM_COMMAND {
                    shutdown(&mut tunnel);
                    tunnel = None;
                } else {
                    error!("Unknown command: {}", command)
                }
            }
            Ok(ReadResult::Binary(buf)) => {
                debug!("Received {} bytes from websocket", buf.len());
                if let Some(s) = tunnel.as_mut() {
                    s.write_all(buf).map_err(|e| FramerError::Io(e)).unwrap();
                    debug!("Send {} bytes to tunnel", buf.len());
                }
            }
            Ok(ReadResult::Pong(_)) => {} // do nothing
            Ok(ReadResult::Closed) => break,
            Err(FramerError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                // a normal timeout
            }
            Err(e) => {
                error!("Framer read error: {:?}", e);
                return Err(e);
            }
        }

        loop {
            match rx_egres.recv_timeout(TIMEOUT) {
                Ok(EgresPayload::Ping) => {
                    let bytes = [];
                    framer.write(&mut stream, WebSocketSendMessageType::Ping, true, &bytes)?
                }
                Ok(EgresPayload::TunnelBytes(bytes)) => {
                    framer.write(&mut stream, WebSocketSendMessageType::Binary, true, &bytes)?
                }
                Err(RecvTimeoutError::Timeout) => {
                    // a normal timeout
                    break;
                }
                Err(e) => {
                    error!("Channel error rx_egres recv: {:?}", &e);
                    return Err(FramerError::Io(std::io::Error::new(ErrorKind::Other, e)));
                }
            }
        }
    }

    info!("Connection closed");
    Ok(())
}

fn shutdown(tunnel: &mut Option<TcpStream>) {
    if let Some(s) = tunnel {
        s.shutdown(Shutdown::Both).unwrap();
    }
}

fn open_tunnel(client_tcp_addr: &str, tx_engres: Sender<EgresPayload>) -> TcpStream {
    let stream = TcpStream::connect(client_tcp_addr)
        .map_err(FramerError::Io)
        .unwrap();

    let mut tunnel_clone = stream.try_clone().expect("Clone tunnel failed");

    // tunnel read loop
    thread::spawn(move || {
        let mut buf = vec![0_u8; 4096 * 4];
        loop {
            // todo: consider using a buffered reader here
            match tunnel_clone.read(&mut buf) {
                Ok(0) => {
                    info!("Tunnel closed");
                    break;
                }
                Ok(len) => {
                    debug!("Received {} bytes from tunnel", len);
                    let bytes = Vec::from(&buf[..len]);
                    tx_engres.send(EgresPayload::TunnelBytes(bytes)).unwrap();
                }
                Err(e) => {
                    error!("Tunnel read error: {:?}", e); // TODO: ignore stream close errors
                    break;
                }
            }
        }
    });

    stream
}
