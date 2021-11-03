use dotenv::dotenv;
use embedded_websocket::{
    framer::{Framer, FramerError, ReadResult, Stream},
    WebSocketClient, WebSocketOptions, WebSocketSendMessageType, WebSocketState,
};
use log::{debug, error, info};
use rand::prelude::ThreadRng;
use std::{
    error::Error,
    net::{Shutdown, TcpStream},
    sync::mpsc::{channel, Sender},
    thread,
    time::Duration,
};
use url::Url;
use ws_tunnel::{CLOSE_STREAM_COMMAND, OPEN_STREAM_COMMAND};

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

    info!("connecting to: {}", address);
    let mut stream = TcpStream::connect(address).map_err(FramerError::Io)?;
    let mut stream_tx1 = stream.try_clone().expect("Unable to clone stream");
    let mut stream_tx2 = stream.try_clone().expect("Unable to clone stream");
    info!("connected.");

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
    framer.connect(&mut stream, &websocket_options)?;

    // ingres is tcp data coming from this machine
    let (tx_ingres, rx_ingres) = channel::<Vec<u8>>();

    // heartbeat
    thread::spawn(move || {
        let mut websocket_tx = WebSocketClient::new_client(rand::thread_rng());
        websocket_tx.state = WebSocketState::Open;
        let mut to_buf = vec![0_u8; 128];
        let from_buf = Vec::new();
        loop {
            write_to_websocket(
                &mut websocket_tx,
                &mut stream_tx2,
                WebSocketSendMessageType::Ping,
                &from_buf,
                &mut to_buf,
            );
            thread::sleep(Duration::from_secs(20))
        }
    });

    // receive bytes comming from internal stream and push them onto the websocket
    thread::spawn(move || {
        let mut websocket_tx = WebSocketClient::new_client(rand::thread_rng());
        websocket_tx.state = WebSocketState::Open;
        let mut to_buf = vec![0_u8; 4096];
        loop {
            match rx_ingres.recv() {
                Ok(tunnel_bytes) => {
                    let len = websocket_tx
                        .write(
                            WebSocketSendMessageType::Binary,
                            true,
                            &tunnel_bytes,
                            &mut to_buf,
                        )
                        .unwrap();
                    stream_tx1.write_all(&to_buf[..len]).unwrap();

                    debug!("sent {} bytes to websocket", tunnel_bytes.len())
                }
                Err(e) => {
                    error!("rx_ingres recv error: {:?}", e);
                    break;
                }
            }
        }
    });

    // receive bytes comming from websocket and push them to the internal stream
    let mut tunnel: Option<TcpStream> = None;
    loop {
        match framer.read(&mut stream, &mut frame_buf) {
            Ok(ReadResult::Text(command)) => {
                info!("received command: {}", command);

                if command == OPEN_STREAM_COMMAND {
                    if let Some(stream) = tunnel.as_mut() {
                        stream.shutdown(Shutdown::Both).unwrap();
                    }

                    tunnel = Some(open_tunnel(&config.client_tcp_addr, tx_ingres.clone()));
                } else if command == CLOSE_STREAM_COMMAND {
                    if let Some(stream) = tunnel.as_mut() {
                        stream.shutdown(Shutdown::Both).unwrap();
                    }

                    tunnel = None;
                } else {
                    error!("unknown command: {}", command)
                }
            }
            Ok(ReadResult::Binary(buf)) => {
                debug!("received {} bytes from websocket", buf.len());
                if let Some(stream) = tunnel.as_mut() {
                    stream
                        .write_all(buf)
                        .map_err(|e| FramerError::Io(e))
                        .unwrap();
                    debug!("send {} bytes to tunnel", buf.len());
                }
            }
            Ok(ReadResult::Pong(_)) => {} // do nothing
            Ok(ReadResult::None) => break,
            Err(e) => {
                error!("framer read error: {:?}", e)
            }
        }
    }

    fn write_to_websocket(
        websocket: &mut WebSocketClient<ThreadRng>,
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

    fn open_tunnel(client_tcp_addr: &str, tx_ingres: Sender<Vec<u8>>) -> TcpStream {
        let stream = TcpStream::connect(client_tcp_addr)
            .map_err(FramerError::Io)
            .unwrap();

        let mut tunnel_clone = stream.try_clone().expect("clone tunnel failed");

        // tunnel read loop
        thread::spawn(move || {
            let mut buf = vec![0_u8; 4096 * 4];
            loop {
                // todo: consider using a buffered reader here
                match tunnel_clone.read(&mut buf) {
                    Ok(0) => {
                        info!("tunnel closed");
                        break;
                    }
                    Ok(len) => {
                        debug!("received {} bytes from tunnel", len);
                        let bytes = Vec::from(&buf[..len]);
                        tx_ingres.send(bytes).unwrap();
                    }
                    Err(e) => {
                        error!("tunnel read error: {:?}", e); // TODO: ignore stream close errors
                        break;
                    }
                }
            }
        });

        stream
    }

    info!("connection closed");
    Ok(())
}
