use dotenv::dotenv;
use embedded_websocket::{
    framer::{Framer, FramerError, ReadResult, Stream},
    WebSocketClient, WebSocketOptions, WebSocketSendMessageType, WebSocketState,
};
use log::{debug, error, info};
use rand::prelude::ThreadRng;
use std::{
    error::Error,
    io::ErrorKind,
    net::{Shutdown, TcpStream},
    sync::mpsc::{channel, Receiver, Sender},
    thread,
    time::Duration,
};
use url::Url;
use ws_tunnel::{TunnelCommand, CLOSE_STREAM_COMMAND, OPEN_STREAM_COMMAND};

fn _open_tunnel_connections(
    rx_egres: Receiver<TunnelCommand>,
    client_tcp_addr: String,
    tx_ingres: Sender<Vec<u8>>,
) {
    let mut tunnel: Option<TcpStream> = None;
    loop {
        // receive websocket messages
        match rx_egres.recv() {
            Ok(TunnelCommand::Open) => {
                if let Some(stream) = tunnel.as_mut() {
                    stream.shutdown(Shutdown::Both).unwrap();
                }

                let stream = TcpStream::connect(&client_tcp_addr)
                    .map_err(FramerError::Io)
                    .unwrap();

                let mut tunnel_clone = stream.try_clone().expect("clone tunnel failed");
                tunnel = Some(stream);

                let tx_ingres_clone = tx_ingres.clone();

                // tunnel read loop
                thread::spawn(move || {
                    let mut buf = vec![0_u8; 4096 * 4];
                    loop {
                        // todo: consider using a buffered reader here
                        let len = tunnel_clone
                            .read(&mut buf)
                            .map_err(FramerError::Io)
                            .unwrap(); // todo, fix this error handling -- we want to be able to exit the thread if the stream is closed
                        debug!("Received {} bytes from tunnel", len);
                        let bytes = Vec::from(&buf[..len]);
                        tx_ingres_clone.send(bytes).unwrap();
                    }
                });
            }
            Ok(TunnelCommand::Close) => {
                if let Some(stream) = tunnel.as_mut() {
                    stream.shutdown(Shutdown::Both).unwrap();
                }

                tunnel = None;
            }
            Ok(TunnelCommand::Tx(bytes)) => {
                if let Some(stream) = tunnel.as_mut() {
                    stream
                        .write_all(&bytes)
                        .map_err(|e| FramerError::Io(e))
                        .unwrap();
                }
            }
            Err(e) => {
                error!("rx_egres recv error: {:?}", e);
                break;
            }
        }
    }
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
    let mut stream = TcpStream::connect(address).map_err(FramerError::Io)?;
    let mut stream_tx1 = stream.try_clone().expect("Unable to clone stream");
    let mut stream_tx2 = stream.try_clone().expect("Unable to clone stream");
    info!("Connected.");

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

    // egres is websocket data coming from outside this machine
    //  let (tx_egres, rx_egres) = channel::<TunnelCommand>();

    // let client_tcp_addr = config.client_tcp_addr.clone();
    // thread::spawn(move || open_tunnel_connections(rx_egres, client_tcp_addr, tx_ingres));

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
            thread::sleep(Duration::from_secs(2))
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

                    debug!("Sent {} bytes", tunnel_bytes.len())
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
                info!("Received command: {}", command);

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
                    error!("Unknown command: {}", command)
                }
            }
            Ok(ReadResult::Binary(buf)) => {
                debug!("Received {} bytes", buf.len());
                if let Some(stream) = tunnel.as_mut() {
                    stream
                        .write_all(buf)
                        .map_err(|e| FramerError::Io(e))
                        .unwrap();
                }
            }
            Ok(ReadResult::Pong(_)) => {} // do nothing
            Ok(ReadResult::None) => break,
            Err(FramerError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                // a normal timeout (TODO: remove this)
            }
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
                        debug!("Received {} bytes from tunnel", len);
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

    //    const TIMEOUT: Duration = Duration::from_millis(50);

    //   stream
    //       .set_read_timeout(Some(TIMEOUT))
    //       .map_err(FramerError::Io)?;
    /*
        loop {
            match framer.read(&mut stream, &mut frame_buf) {
                Ok(ReadResult::Text(command)) => {
                    info!("Received command: {}", command);

                    if command == OPEN_STREAM_COMMAND {
                        tx_egres.send(TunnelCommand::Open).unwrap()
                    } else if command == CLOSE_STREAM_COMMAND {
                        tx_egres.send(TunnelCommand::Close).unwrap()
                    } else {
                        error!("Unknown command: {}", command)
                    }
                }
                Ok(ReadResult::Binary(buf)) => {
                    debug!("Received: {} bytes", buf.len());
                    let bytes = Vec::from(buf);
                    tx_egres.send(TunnelCommand::Tx(bytes)).unwrap();
                }
                Ok(ReadResult::Pong(_)) => {} // do nothing
                Ok(ReadResult::None) => break,
                Err(FramerError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    // a normal timeout (TODO: remove this)
                }
                Err(e) => {
                    error!("framer read error: {:?}", e)
                }
            }
        }
    */
    info!("Connection closed");
    Ok(())
}
