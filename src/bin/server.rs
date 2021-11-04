// The MIT License (MIT)
// Copyright (c) 2021 David Haig

use dotenv::dotenv;
use embedded_websocket as ws;
use httparse::Header;
use log::{debug, error, info};
use std::net::{TcpListener, TcpStream};
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

use ws_tunnel::{TunnelCommand, CLOSE_STREAM_COMMAND, OPEN_STREAM_COMMAND};

type Result<T> = std::result::Result<T, WebServerError>;

#[derive(Debug)]
pub enum WebServerError {
    Io(std::io::Error),
    Framer(FramerError<std::io::Error>),
    WebSocket(ws::Error),
    Utf8Error,
}

impl From<std::io::Error> for WebServerError {
    fn from(err: std::io::Error) -> WebServerError {
        WebServerError::Io(err)
    }
}

impl From<FramerError<std::io::Error>> for WebServerError {
    fn from(err: FramerError<std::io::Error>) -> WebServerError {
        WebServerError::Framer(err)
    }
}

impl From<ws::Error> for WebServerError {
    fn from(err: ws::Error) -> WebServerError {
        WebServerError::WebSocket(err)
    }
}

impl From<Utf8Error> for WebServerError {
    fn from(_: Utf8Error) -> WebServerError {
        WebServerError::Utf8Error
    }
}

fn accept_tunnel_connections(
    tunnel_listener: TcpListener,
    tx_ingres: Sender<TunnelCommand>,
    rx_egres: Receiver<Vec<u8>>,
    tx_egres: Sender<Vec<u8>>,
) {
    for stream in tunnel_listener.incoming() {
        match stream {
            Ok(mut stream) => {
                info!("incomming tunnel connection from {:?}", stream.peer_addr());
                let mut stream_clone = stream.try_clone().unwrap();
                let tx_ingres_clone = tx_ingres.clone();
                tx_ingres_clone.send(TunnelCommand::Open).unwrap();
                let tx_egres_clone = tx_egres.clone();
                thread::spawn(move || {
                    info!("reading bytes from tunnel and forwarding them to websocket");
                    let mut buf = vec![0_u8; 4 * 4096];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) => {
                                info!("tunnel closed");
                                tx_ingres_clone.send(TunnelCommand::Close).unwrap();
                                tx_egres_clone.send(Vec::new()).unwrap(); // send an empty packet
                                break;
                            }
                            Ok(len) => {
                                let bytes = Vec::from(&buf[..len]);
                                tx_ingres_clone.send(TunnelCommand::Tx(bytes)).unwrap();
                            }
                            Err(e) => {
                                error!("tunnel error: {:?}", e);
                                tx_ingres_clone.send(TunnelCommand::Close).unwrap();
                                break;
                            }
                        }
                    }
                });

                info!("waiting for websocket bytes to forward");

                let mut has_received_data = false;

                loop {
                    match rx_egres.recv() {
                        Ok(bytes) => {
                            if bytes.len() == 0 {
                                if has_received_data {
                                    info!("rx_egres received 0 bytes and quitting");
                                    break;
                                } else {
                                    info!("rx_egres received 0 bytes but continuing to receive");
                                    continue;
                                }
                            }

                            debug!("rx_egres received {} bytes", bytes.len());
                            has_received_data = true;

                            if let Err(e) = stream_clone.write_all(&bytes) {
                                error!("stream write error: {:?}", e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("rx_egres read error: {:?}", e);
                            break;
                        }
                    }
                }

                info!("no longer forwarding websocket bytes to tunnel");
            }
            Err(e) => error!("failed to establish a connection: {}", e),
        }
    }
}

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // load config from .env file
    dotenv().ok();
    let config = ws_tunnel::config::Config::from_env().expect("Invalid config");

    let listener = TcpListener::bind(&config.server_ws_addr)?;
    info!("websocket listening on: {}", &config.server_ws_addr);

    let tunnel_listener = TcpListener::bind(&config.server_tcp_addr)?;
    info!("tunnel listening on: {}", &config.server_tcp_addr);

    let (tx_ingres, rx_ingres) = channel::<TunnelCommand>();
    let (tx_egres, rx_egres) = channel::<Vec<u8>>();

    let tx_egres_clone = tx_egres.clone();
    let tx_ingres_clone = tx_ingres.clone();
    thread::spawn(move || {
        accept_tunnel_connections(tunnel_listener, tx_ingres_clone, rx_egres, tx_egres_clone);
        info!("stopped accepting tunnel connections")
    });

    let rx_ingres_mutex = Arc::new(Mutex::new(rx_ingres));

    // accept connections and process them serially
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                info!(
                    "incomming websocket connection from {:?}",
                    stream.peer_addr()
                );

                let api_key = config.api_key.clone();
                let rx_ingres_mutex = rx_ingres_mutex.clone();
                let tx_egres = tx_egres.clone();
                let tx_ingres = tx_ingres.clone();
                thread::spawn(move || {
                    match handle_client(stream, tx_egres, rx_ingres_mutex, tx_ingres, &api_key) {
                        Ok(()) => info!("connection closed"),
                        Err(e) => error!("handle_client error: {:?}", e),
                    }
                });
            }
            Err(e) => error!("failed to establish a connection: {}", e),
        }
    }

    Ok(())
}

fn handle_client(
    mut stream: TcpStream,
    tx_egres: Sender<Vec<u8>>,
    rx_ingres_mutex: Arc<Mutex<Receiver<TunnelCommand>>>,
    tx_ingres: Sender<TunnelCommand>,
    api_key: &String,
) -> Result<()> {
    info!("client connected {}", stream.peer_addr()?);
    let mut read_buf = [0; 4000];
    let mut read_cursor = 0;

    if let Some(websocket_context) =
        read_header(&mut stream, &mut read_buf, &mut read_cursor, api_key)?
    {
        // this is a websocket upgrade HTTP request
        let mut write_buf = [0; 4000];
        let mut frame_buf = [0; 4000];
        let mut websocket = WebSocketServer::new_server();
        let mut framer = Framer::new(
            &mut read_buf,
            &mut read_cursor,
            &mut write_buf,
            &mut websocket,
        );

        // complete the opening handshake with the client
        framer.accept(&mut stream, &websocket_context)?;
        info!("websocket connection opened");

        let mut stream_cloned = stream.try_clone().expect("unable to clone stream");
        thread::spawn(move || {
            let mut to_buf = vec![0_u8; 4 * 4096];
            let mut websocket_tx = WebSocketServer::new_server();
            websocket_tx.state = WebSocketState::Open;

            loop {
                let rx_ingres = rx_ingres_mutex.lock().unwrap();

                // attempt to read from the channel and write websocket frame
                match rx_ingres.recv() {
                    Ok(TunnelCommand::Open) => {
                        info!("sending 'open' stream message to websocket");
                        write_to_websocket(
                            &mut websocket_tx,
                            &mut stream_cloned,
                            WebSocketSendMessageType::Text,
                            OPEN_STREAM_COMMAND.as_bytes(),
                            &mut to_buf,
                        );
                    }
                    Ok(TunnelCommand::Close) => {
                        info!("sending 'close' stream message to websocket");
                        write_to_websocket(
                            &mut websocket_tx,
                            &mut stream_cloned,
                            WebSocketSendMessageType::Text,
                            CLOSE_STREAM_COMMAND.as_bytes(),
                            &mut to_buf,
                        );
                    }
                    Ok(TunnelCommand::Tx(bytes)) => {
                        debug!("sending {} bytes to websocket", bytes.len());

                        write_to_websocket(
                            &mut websocket_tx,
                            &mut stream_cloned,
                            WebSocketSendMessageType::Binary,
                            &bytes,
                            &mut to_buf,
                        );
                    }
                    Ok(TunnelCommand::ClientDisconnected) => {
                        info!("client disconnected, exiting websocket writer loop");
                        break;
                    }
                    Err(e) => {
                        error!("rx_ingres disconnected: {:?}", e);
                        break;
                    }
                }
            }
        });

        // read loop
        loop {
            // attempt to read a websocket frame
            match framer.read(&mut stream, &mut frame_buf) {
                Ok(ReadResult::Binary(bytes)) => {
                    debug!("received {} bytes from websocket", bytes.len());
                    let bytes = Vec::from(bytes);
                    if let Err(e) = tx_egres.send(bytes) {
                        // this will only happen if we stop accepting incomming tcp connections for some reason
                        error!("tx_egres send error: {:?}", e);
                    }
                }
                Ok(ReadResult::Text(_)) => {} // do nothing
                Ok(ReadResult::Pong(_)) => {} // do nothing
                Ok(ReadResult::Closed) => {
                    info!("client closed websocket connection");
                    tx_egres.send(Vec::new()).unwrap();
                    tx_ingres.send(TunnelCommand::ClientDisconnected).unwrap();
                    break; // usually when client has closed the connection
                }
                Err(e) => {
                    error!("read_binary error: {:?}", e);
                    break;
                }
            }
        }

        info!("closing websocket connection");
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

fn read_authorization_header<'a>(headers: &[Header]) -> Result<Option<String>> {
    for header in headers {
        if header.name == "Authorization" {
            let value = from_utf8(header.value)?;
            const BEARER: &'static str = "Bearer ";
            if value.starts_with(BEARER) && value.len() > BEARER.len() {
                let api_key = value[BEARER.len()..].to_owned();
                return Ok(Some(api_key));
            }
        }
    }

    Ok(None)
}

fn read_header(
    stream: &mut TcpStream,
    read_buf: &mut [u8],
    read_cursor: &mut usize,
    api_key: &String,
) -> Result<Option<WebSocketContext>> {
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
                let header_api_key = read_authorization_header(&request.headers)?;

                let headers = request.headers.iter().map(|f| (f.name, f.value));
                match ws::read_http_header(headers)? {
                    Some(websocket_context) => match request.path {
                        Some("/ws-gateway-tunnel") => {
                            if let Some(header_api_key) = header_api_key {
                                if &header_api_key == api_key {
                                    return Ok(Some(websocket_context));
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
    info!("unauthorized");
    let html = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(&html.as_bytes())?;
    Ok(())
}

fn return_404_not_found(stream: &mut TcpStream, unknown_path: Option<&str>) -> Result<()> {
    thread::sleep(Duration::from_secs(2));
    info!("unknown path: {:?}", unknown_path);
    let html = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(&html.as_bytes())?;
    Ok(())
}
