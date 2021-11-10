// The MIT License (MIT)
// Copyright (c) 2021 David Haig

use base64::DecodeError;
use embedded_websocket as ws;
use httparse::Header;
use log::{debug, error, info, warn};
use std::any::Any;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::str::{from_utf8, Utf8Error};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::{
    io::{Read, Write},
    usize,
};
use ws::framer::ReadResult;
use ws::WebSocketState;
use ws::{
    framer::{Framer, FramerError},
    WebSocketContext, WebSocketSendMessageType, WebSocketServer,
};
use ws_tunnel::config::{ConfigError, ServerConfig};

use ws_tunnel::{EgresCommand, TunnelCommand, CLOSE_STREAM_COMMAND, OPEN_STREAM_COMMAND};
type Result<T> = core::result::Result<T, ServerError>;

#[derive(Debug)]
pub enum ServerError {
    Io(std::io::Error),
    Framer(FramerError<std::io::Error>),
    WebSocket(ws::Error),
    Utf8Error,
    Config(ConfigError),
    Base64(DecodeError),
    AcceptConnectionThread(Box<dyn Any + Send>),
    WriteThread(Box<dyn Any + Send>),
}

impl From<std::io::Error> for ServerError {
    fn from(err: std::io::Error) -> ServerError {
        ServerError::Io(err)
    }
}

impl From<FramerError<std::io::Error>> for ServerError {
    fn from(err: FramerError<std::io::Error>) -> ServerError {
        ServerError::Framer(err)
    }
}

impl From<ws::Error> for ServerError {
    fn from(err: ws::Error) -> ServerError {
        ServerError::WebSocket(err)
    }
}

impl From<Utf8Error> for ServerError {
    fn from(_: Utf8Error) -> ServerError {
        ServerError::Utf8Error
    }
}

fn handle_tunnel_connection(
    mut stream: TcpStream,
    tx_ingres: &mut Sender<TunnelCommand>,
    rx_egres: &mut Receiver<EgresCommand>,
    tx_egres: &mut Sender<EgresCommand>,
) -> Result<bool> {
    info!("Incomming tunnel connection from {:?}", stream.peer_addr());
    let mut stream_clone = stream.try_clone().unwrap(); // this should not fail for tcp streams
    let tx_ingres_clone = tx_ingres.clone();
    tx_ingres_clone.send(TunnelCommand::Open).ok();
    let tx_egres_clone = tx_egres.clone();

    thread::spawn(move || {
        info!("Reading bytes from tunnel and forwarding them to websocket");
        let mut buf = vec![0_u8; 4000];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    info!("Tunnel closed");
                    tx_ingres_clone.send(TunnelCommand::Close).ok();
                    tx_egres_clone.send(EgresCommand::Close).ok();
                    break;
                }
                Ok(len) => {
                    let bytes = Vec::from(&buf[..len]);
                    tx_ingres_clone.send(TunnelCommand::Tx(bytes)).ok();
                }
                Err(e) => {
                    error!("Tunnel error: {:?}", e);
                    tx_ingres_clone.send(TunnelCommand::Close).ok();
                    tx_egres_clone.send(EgresCommand::Close).ok();
                    break;
                }
            }
        }

        info!("No longer reading bytes from tunnel");
    });

    info!("Waiting for websocket bytes to forward");

    loop {
        match rx_egres.recv() {
            Ok(EgresCommand::Bytes(bytes)) => {
                debug!("Channel rx_egres received {} bytes", bytes.len());
                if let Err(e) = stream_clone.write_all(&bytes) {
                    error!("Stream write error: {:?}", e);
                    return Ok(false);
                }
            }
            Ok(EgresCommand::Close) => {
                info!("Received EgresCommand Close");
                return Ok(true);
            }
            Ok(EgresCommand::WebsocketClosed) => {
                info!("Received EgresCommand WebsocketClosed");
                stream_clone.shutdown(Shutdown::Both)?;
                return Ok(false);
            }
            Err(e) => {
                error!("Channel rx_egres read error: {:?}", e);
                return Ok(false);
            }
        }
    }
}

fn accept_tunnel_connections(
    tunnel_listener: TcpListener,
    mut tx_ingres: Sender<TunnelCommand>,
    mut rx_egres: Receiver<EgresCommand>,
    mut tx_egres: Sender<EgresCommand>,
) -> Result<()> {
    for stream in tunnel_listener.incoming() {
        match stream {
            Ok(stream) => {
                match handle_tunnel_connection(stream, &mut tx_ingres, &mut rx_egres, &mut tx_egres)
                {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => Err(e)?,
                }
            }
            Err(e) => {
                error!("Failed to establish a connection: {}", e);
                Err(e)?
            }
        }
    }

    info!("No longer accepting tunnel connections");
    Ok(())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // load config from .env file
    let config = ws_tunnel::config::ServerConfig::from_file().map_err(ServerError::Config)?;

    let listener = TcpListener::bind(&config.ws_addr)?;
    info!("Websocket listening on: {}", &config.ws_addr);

    // accept connections and process them serially
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                info!(
                    "Incomming websocket connection from {:?}",
                    stream.peer_addr()
                );

                let cfg = config.clone();

                thread::spawn(move || match handle_client(stream, cfg) {
                    Ok(()) => info!("Connection closed"),
                    Err(e) => error!("Http handle_client error: {:?}", e),
                });
            }
            Err(e) => error!("Failed to establish a connection: {}", e),
        }
    }

    Ok(())
}

fn write_to_websocket_loop(
    mut stream: TcpStream,
    rx_ingres_mutex: Arc<Mutex<Receiver<TunnelCommand>>>,
) -> Result<()> {
    let mut to_buf = vec![0_u8; 4 * 4096];
    let mut websocket_tx = WebSocketServer::new_server();
    websocket_tx.state = WebSocketState::Open;

    loop {
        let rx_ingres = rx_ingres_mutex.lock().unwrap();

        // attempt to read from the channel and write websocket frame
        match rx_ingres.recv() {
            Ok(TunnelCommand::Open) => {
                info!("Sending 'open' stream message to websocket");
                write_to_websocket(
                    &mut websocket_tx,
                    &mut stream,
                    WebSocketSendMessageType::Text,
                    OPEN_STREAM_COMMAND.as_bytes(),
                    &mut to_buf,
                );
            }
            Ok(TunnelCommand::Close) => {
                info!("Sending 'close' stream message to websocket");
                write_to_websocket(
                    &mut websocket_tx,
                    &mut stream,
                    WebSocketSendMessageType::Text,
                    CLOSE_STREAM_COMMAND.as_bytes(),
                    &mut to_buf,
                );
            }
            Ok(TunnelCommand::Tx(bytes)) => {
                debug!("Sending {} bytes to websocket", bytes.len());

                write_to_websocket(
                    &mut websocket_tx,
                    &mut stream,
                    WebSocketSendMessageType::Binary,
                    &bytes,
                    &mut to_buf,
                );
            }
            Ok(TunnelCommand::ClientDisconnected) => {
                info!("Client disconnected, exiting websocket writer loop");
                break;
            }
            Err(e) => {
                error!("Channel rx_ingres disconnected: {:?}", e);
                break;
            }
        }
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, config: ServerConfig) -> Result<()> {
    info!("Client connected {}", stream.peer_addr()?);
    let mut read_buf = vec![0_u8; 4000];
    let mut read_cursor = 0;

    if let Some(header) = read_header(&mut stream, &mut read_buf, &mut read_cursor, &config)? {
        let (tx_ingres, rx_ingres) = channel::<TunnelCommand>();
        let (tx_egres, rx_egres) = channel::<EgresCommand>();

        // this is a websocket upgrade HTTP request
        let mut write_buf = vec![0_u8; 4096];
        let mut frame_buf = vec![0_u8; 4096];
        let mut websocket = WebSocketServer::new_server();
        let mut framer = Framer::new(
            &mut read_buf,
            &mut read_cursor,
            &mut write_buf,
            &mut websocket,
        );

        // complete the opening handshake with the client
        framer.accept(&mut stream, &header.websocket_context)?;
        info!("Websocket connection opened for user {}", &header.user);

        let tunnel_listener = TcpListener::bind(&header.tunnel_addr)?;
        info!("Tunnel listening on: {}", &header.tunnel_addr);

        // accept tunnel connection loop
        let tx_egres_clone = tx_egres.clone();
        let tx_ingres_clone = tx_ingres.clone();
        let accept_connection_thread = thread::spawn(move || -> Result<()> {
            accept_tunnel_connections(tunnel_listener, tx_ingres_clone, rx_egres, tx_egres_clone)
        });

        let rx_ingres_mutex = Arc::new(Mutex::new(rx_ingres));

        // write to websocket loop
        let stream_cloned = stream.try_clone().unwrap(); // will always work
        let write_thread = thread::spawn(move || -> Result<()> {
            write_to_websocket_loop(stream_cloned, rx_ingres_mutex)
        });

        // read loop
        loop {
            // attempt to read a websocket frame
            match framer.read(&mut stream, &mut frame_buf) {
                Ok(ReadResult::Binary(bytes)) => {
                    debug!("Received {} bytes from websocket", bytes.len());
                    let bytes = Vec::from(bytes);
                    if let Err(e) = tx_egres.send(EgresCommand::Bytes(bytes)) {
                        // this will only happen if we stop accepting incomming tcp connections for some reason
                        error!("Channel tx_egres send error: {:?}", e);
                        break;
                    }
                }
                Ok(ReadResult::Text(_)) => {} // do nothing
                Ok(ReadResult::Pong(_)) => {} // do nothing
                Ok(ReadResult::Closed) => {
                    info!("Client closed websocket connection");
                    break; // usually when client has closed the connection
                }
                Err(e) => {
                    error!("Framer read_binary error: {:?}", e);
                    break;
                }
            }
        }

        tx_ingres.send(TunnelCommand::ClientDisconnected).ok();
        info!("Stop writing websocket messages");
        let join_write_thread = write_thread.join().map_err(ServerError::WriteThread);

        tx_egres.send(EgresCommand::WebsocketClosed).ok();
        info!("Stop accepting connections");
        let join_accept_connection_thread = accept_connection_thread
            .join()
            .map_err(ServerError::AcceptConnectionThread);

        join_write_thread??;
        join_accept_connection_thread??;
        info!("Websocket connection closed");
        Ok(())
    } else {
        Ok(())
    }
}

fn write_to_websocket(
    websocket: &mut WebSocketServer,
    stream: &mut TcpStream,
    message_type: WebSocketSendMessageType,
    from_buf: &[u8],
    to_buf: &mut [u8],
) {
    // TODO: write in loop if buffer is too small
    let len = websocket
        .write(message_type, true, from_buf, to_buf)
        .unwrap();
    stream.write_all(&to_buf[..len]).unwrap();
}

fn read_authorization_header<'a>(headers: &[Header]) -> Result<Option<(String, String)>> {
    for header in headers {
        if header.name == "Authorization" {
            if let Ok(value) = from_utf8(header.value) {
                const BASIC: &'static str = "Basic ";
                if value.starts_with(BASIC) && value.len() > BASIC.len() {
                    let encoded = value[BASIC.len()..].to_owned();
                    let encoded = encoded.as_bytes();
                    let decoded = match base64::decode(encoded) {
                        Ok(x) => x,
                        Err(_) => {
                            warn!("Authorization header not base64 encoded");
                            return Ok(None);
                        }
                    };

                    let decoded = match from_utf8(&decoded) {
                        Ok(x) => x,
                        Err(_) => {
                            warn!("Authorization header Utf8 error");
                            return Ok(None);
                        }
                    };

                    if let Some((username, password)) = decoded.split_once(":") {
                        let pair = (username.to_owned(), password.to_owned());
                        return Ok(Some(pair));
                    }
                }
            }
        }
    }

    Ok(None)
}

struct WebSocketHeader {
    tunnel_addr: String,
    user: String,
    websocket_context: WebSocketContext,
}

fn read_header(
    stream: &mut TcpStream,
    read_buf: &mut [u8],
    read_cursor: &mut usize,
    config: &ServerConfig,
) -> Result<Option<WebSocketHeader>> {
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut request = httparse::Request::new(&mut headers);

        let received_size = stream.read(&mut read_buf[*read_cursor..])?;

        match request
            .parse(&read_buf[..*read_cursor + received_size])
            .unwrap()
        {
            httparse::Status::Complete(len) => {
                // if we read exactly the right amount of bytes for the HTTP header then read_cursor would be 0
                *read_cursor += received_size - len;
                let auth = read_authorization_header(&request.headers)?;

                let headers = request.headers.iter().map(|f| (f.name, f.value));
                match ws::read_http_header(headers)? {
                    Some(websocket_context) => match request.path {
                        Some(path) if path == config.ws_path => {
                            if let Some(pair) = auth {
                                for client in config.clients.iter() {
                                    if client.username == pair.0 && client.password == pair.1 {
                                        return Ok(Some(WebSocketHeader {
                                            user: client.username.clone(),
                                            tunnel_addr: client.tcp_addr.clone(),
                                            websocket_context,
                                        }));
                                    }
                                }
                            }

                            return_401_unauthorized(stream)?
                        }
                        _ => return_404_not_found(stream, request.path)?,
                    },
                    None => return_404_not_found(stream, request.path)?,
                }

                return Ok(None);
            }

            // keep reading while the HTTP header is incomplete
            httparse::Status::Partial => *read_cursor += received_size,
        }
    }
}

fn return_401_unauthorized(stream: &mut TcpStream) -> Result<()> {
    thread::sleep(Duration::from_secs(2));
    info!("Unauthorized");
    let html = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(&html.as_bytes())?;
    Ok(())
}

fn return_404_not_found(stream: &mut TcpStream, unknown_path: Option<&str>) -> Result<()> {
    thread::sleep(Duration::from_secs(2));
    info!("Unknown path: {:?}", unknown_path);
    let html = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(&html.as_bytes())?;
    Ok(())
}
