use embedded_websocket::{
    framer::{Framer, FramerError, ReadResult, Stream},
    WebSocketClient, WebSocketOptions, WebSocketSendMessageType,
};
use log::{debug, error, info};
use rustls::{self, ClientConnection, ServerName};
use rustls::{OwnedTrustAnchor, RootCertStore};
use std::sync::Arc;
use std::{borrow::BorrowMut, convert::TryInto};
use std::{
    io::ErrorKind,
    net::{Shutdown, TcpStream},
    sync::mpsc::{channel, RecvTimeoutError, Sender},
    thread,
    time::Duration,
};
use url::Url;
use webpki_roots;
use ws_tunnel::{CLOSE_STREAM_COMMAND, OPEN_STREAM_COMMAND};

struct ClientStream {
    stream: TcpStream,
    connection: Option<ClientConnection>,
}

impl<'a> Stream<std::io::Error> for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        if let Some(connection) = self.connection.borrow_mut() {
            let mut tls = rustls::Stream::new(connection, &mut self.stream);
            std::io::Read::read(&mut tls, buf)
        } else {
            std::io::Read::read(&mut self.stream, buf)
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        if let Some(connection) = self.connection.borrow_mut() {
            let mut tls = rustls::Stream::new(connection, &mut self.stream);
            std::io::Write::write_all(&mut tls, buf)
        } else {
            std::io::Write::write_all(&mut self.stream, buf)
        }
    }
}

enum EgresPayload {
    Ping,
    TunnelBytes(Vec<u8>),
}

#[derive(Debug)]
enum ClientError {
    Framer(FramerError<std::io::Error>),
    Config(ws_tunnel::config::ConfigError),
    Io(std::io::Error),
    Tls(rustls::Error),
    Url(String),
}

impl From<FramerError<std::io::Error>> for ClientError {
    fn from(e: FramerError<std::io::Error>) -> Self {
        ClientError::Framer(e)
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

fn main() -> Result<(), ClientError> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = ws_tunnel::config::ClientConfig::from_file().map_err(ClientError::Config)?;
    let url = Url::parse(&config.ws_url).expect("Invalid client_ws_url");
    let host = url.host_str().expect("Invalid url host name");

    info!("scheme {} port {:?}", url.scheme(), url.port());

    let port = match url.port() {
        Some(port) => port,
        None => match url.scheme() {
            "wss" => 433,
            "https" => 433,
            "ws" => 80,
            "http" => 80,
            _ => return Err(ClientError::Url("Invalid url, unknown port".to_owned())),
        },
    };

    let use_tls = url.scheme() == "wss" || url.scheme() == "https";
    let tcp_address = format!("{}:{}", host, port);

    info!("Connecting to: {}, use tls: {}", tcp_address, use_tls);
    const TIMEOUT: Duration = Duration::from_millis(50);

    let connection = if use_tls {
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

        let server_name: ServerName = host
            .try_into()
            .expect(&format!("Address '{}' is not a valid ServerName", host));
        Some(
            rustls::ClientConnection::new(Arc::new(tls_config), server_name)
                .map_err(ClientError::Tls)?,
        )
    } else {
        None
    };

    let stream = TcpStream::connect(tcp_address)?;
    stream.set_read_timeout(Some(TIMEOUT))?;

    let mut stream = ClientStream { connection, stream };

    info!("Tcp connected");

    let mut read_buf = [0; 4000];
    let mut read_cursor = 0;
    let mut write_buf = [0; 4000];
    let mut frame_buf = [0; 4000];
    let mut websocket = WebSocketClient::new_client(rand::thread_rng());

    let encoded_credentials = base64::encode(format!("{}:{}", config.username, config.password));
    let auth = format!("Authorization: Basic {}", encoded_credentials);
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
            Err(e) => return Err(ClientError::Framer(e)),
        }
    }

    info!("Websocket connected");

    // egres is tcp data leaving this machine via websocket
    let (tx_egres, rx_egres) = channel::<EgresPayload>();
    let tx_egres_clone = tx_egres.clone();

    // heartbeat
    thread::spawn(move || loop {
        tx_egres_clone
            .send(EgresPayload::Ping)
            .expect("Failed to send heartbeat");
        thread::sleep(Duration::from_secs(20))
    });

    // receive bytes comming from websocket and push them to the internal stream
    let mut tunnel: Option<TcpStream> = None;
    loop {
        match framer.read(&mut stream, &mut frame_buf) {
            Ok(ReadResult::Text(command)) => {
                info!("Received command: {}", command);

                if command == OPEN_STREAM_COMMAND {
                    shutdown(&mut tunnel)?;
                    let new_stream = open_tunnel(&config.tcp_addr, tx_egres.clone())?;
                    tunnel = Some(new_stream);
                } else if command == CLOSE_STREAM_COMMAND {
                    shutdown(&mut tunnel)?;
                    tunnel = None;
                } else {
                    error!("Unknown command: {}", command)
                }
            }
            Ok(ReadResult::Binary(buf)) => {
                debug!("Received {} bytes from websocket", buf.len());
                if let Some(s) = tunnel.as_mut() {
                    s.write_all(buf)?;
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
                return Err(ClientError::Framer(e));
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
                    return Err(ClientError::Io(std::io::Error::new(ErrorKind::Other, e)));
                }
            }
        }
    }

    info!("Connection closed");
    Ok(())
}

fn shutdown(tunnel: &mut Option<TcpStream>) -> Result<(), ClientError> {
    if let Some(s) = tunnel {
        s.shutdown(Shutdown::Both)?;
    }

    Ok(())
}

fn open_tunnel(
    client_tcp_addr: &str,
    tx_engres: Sender<EgresPayload>,
) -> Result<TcpStream, ClientError> {
    let stream = TcpStream::connect(client_tcp_addr)?;

    let mut tunnel_clone = stream.try_clone().expect("Clone tunnel failed");

    // tunnel read loop
    thread::spawn(move || {
        let mut buf = vec![0_u8; 4096 * 4];
        loop {
            // TODO: consider using a buffered reader here
            match tunnel_clone.read(&mut buf) {
                Ok(0) => {
                    info!("Tunnel closed");
                    break;
                }
                Ok(len) => {
                    debug!("Received {} bytes from tunnel", len);
                    let bytes = Vec::from(&buf[..len]);
                    tx_engres
                        .send(EgresPayload::TunnelBytes(bytes))
                        .expect("Failed to send egres bytes");
                }
                Err(e) => {
                    error!("Tunnel read error: {:?}", e); // TODO: ignore stream close errors
                    break;
                }
            }
        }
    });

    Ok(stream)
}
