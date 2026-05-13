use std::convert::TryFrom;

use ::tokio::io::{AsyncReadExt, AsyncWriteExt};
use ::tokio::net::TcpStream;
use ::tokio::sync::mpsc::{self, Receiver, error::TrySendError};
use ::tracing::{error, info, warn};

use super::protocol::*;

const HS_IN: &[u8] = b"Vivat Sicher, Rock'n'Roll forever!!!";
const HS_OUT: &[u8] = b"Enter, my son, please...";

pub type ClientID = usize;

pub struct MpscData(pub ClientID, pub Connection);

pub enum Connection {
    Connected,
    // authenticated with protocol version
    Authenticated(u8),
    Disconnected,
    Updated(Packet),
}

impl PartialEq for Connection {
    fn eq(&self, other: &Self) -> bool {
        use Connection::Authenticated as A;
        use Connection::Connected as C;
        use Connection::Disconnected as D;

        matches!((self, other), (A(_), A(_)) | (C, C) | (D, D))
    }
}

pub struct Client {
    /// Uniq ClientID
    pub id: ClientID,
    pub connection: Connection,
    pub protocol: u8,
    tx_server: mpsc::Sender<MpscData>,
    tx_client: mpsc::Sender<Vec<u8>>,
}

impl Client {
    pub fn send(&self, packet: &Packet) {
        if packet.action.is_lossy_realtime() {
            self.send_lossy(packet);
        } else {
            self.send_reliable(packet);
        }
    }

    pub fn send_reliable(&self, packet: &Packet) {
        let action = packet.action;
        let data = packet.as_bytes();

        match self.tx_client.try_send(data) {
            Ok(()) => {}
            Err(TrySendError::Full(data)) => {
                let tx_client = self.tx_client.clone();
                let client_id = self.id;
                warn!(
                    client_id,
                    action = ?action,
                    "client outbound queue is full; scheduling reliable packet"
                );
                ::tokio::spawn(async move {
                    if tx_client.send(data).await.is_err() {
                        warn!(
                            client_id,
                            action = ?action,
                            "client outbound queue is closed; reliable packet dropped"
                        );
                    }
                });
            }
            Err(TrySendError::Closed(_)) => {
                warn!(
                    client_id = self.id,
                    action = ?action,
                    "client outbound queue is closed; reliable packet dropped"
                );
            }
        }
    }

    pub fn send_lossy(&self, packet: &Packet) {
        let action = packet.action;

        match self.tx_client.try_send(packet.as_bytes()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                warn!(
                    client_id = self.id,
                    action = ?action,
                    "client outbound queue is full; dropping lossy realtime packet"
                );
            }
            Err(TrySendError::Closed(_)) => {
                warn!(
                    client_id = self.id,
                    action = ?action,
                    "client outbound queue is closed; dropping lossy realtime packet"
                );
            }
        }
    }

    fn event_loop(&self, mut stream: TcpStream, mut rx_server: Receiver<Vec<u8>>) {
        let tx_server = self.tx_server.clone();
        let id = self.id;
        let peer = stream
            .peer_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());

        ::tokio::spawn(async move {
            let protocol = match auth(&mut stream).await {
                Ok(protocol_version) => protocol_version,
                Err(err) => {
                    info!(peer=%peer, "auth failed: {}", err);
                    if let Err(write_err) = stream.write_all(b"Auth failed, bye-bye\0").await {
                        warn!(peer=%peer, "failed to send auth failure reply: {write_err:?}");
                    }
                    if let Err(shutdown_err) = stream.shutdown().await {
                        warn!(peer=%peer, "failed to shutdown auth-failed socket: {shutdown_err:?}");
                    }
                    if tx_server
                        .send(MpscData(id, Connection::Disconnected))
                        .await
                        .is_err()
                    {
                        warn!(peer=%peer, "Can't send `Connection::Disconnected` event to server receiver");
                    }
                    return;
                }
            };

            if tx_server
                .send(MpscData(id, Connection::Authenticated(protocol)))
                .await
                .is_err()
            {
                warn!("Can't send `Connection::Auth` event to server receiver");
                return;
            }

            let (mut sr, mut sw) = stream.into_split();

            ::tokio::spawn(async move {
                while let Some(data) = rx_server.recv().await {
                    if let Err(err) = sw.write_all(&data).await {
                        error!("client::event_loop: error sending data to client: {err:?}");
                        break;
                    }
                }
            });

            let mut buff = [0u8; i16::MAX as usize];
            let mut buff_offset: usize = 0;
            loop {
                match sr.read(&mut buff[buff_offset..]).await {
                    Ok(0) => {
                        info!(peer=%peer, "Connection closed by client");
                        if tx_server
                            .send(MpscData(id, Connection::Disconnected))
                            .await
                            .is_err()
                        {
                            warn!(peer=%peer, "Can't send `Connection::Disconnected` event to server receiver");
                        }
                        break;
                    }
                    Ok(n) => {
                        // let event_size = (buff[0] as u16) as i16 | ((buff[1] as i16) << 8);
                        // if event_size < 0 {
                        //     panic!("Warning: event_size is less than zero");
                        // }
                        // if event_size > i16::try_from(n + 2).unwrap() {
                        //     println!("Warning: event_size is bigger than size of the income data");
                        //     dbg!(event_size, Action::from_u8(buff[2]));
                        // }

                        // total bytes with data: `buff_offset` from previous fetching, `n` - current fetching
                        let buff_readable_size = n + buff_offset;

                        let mut offset = 0;
                        let mut i = 0;
                        while offset < buff_readable_size {
                            i += 1;
                            let packet_size =
                                2 + ((buff[offset] as i16) | ((buff[offset + 1] as i16) << 8));
                            let packet_size = match usize::try_from(packet_size) {
                                Ok(packet_size) => packet_size,
                                Err(_) => {
                                    error!("=================== ERROR ==================");
                                    error!("packet_size < 0, iteration: {}", i);
                                    error!("packet.data.parsed: {:?}", buff[..offset].to_vec());
                                    error!("packet.data.failed: {:?}", buff[offset..].to_vec());
                                    break;
                                }
                            };

                            if packet_size > buff_readable_size - offset {
                                // a tail of the packet will be expect by next reading from the socket
                                // removes all parsed bytes and resets buff_offset
                                buff.copy_within(offset..buff_readable_size, 0);
                                for b in &mut buff[buff_readable_size - offset..] {
                                    *b = 0;
                                }
                                break;
                            }

                            let p = Packet::from_slice(&buff[offset..offset + packet_size]);

                            if tx_server
                                .send(MpscData(id, Connection::Updated(p)))
                                .await
                                .is_err()
                            {
                                panic!(
                                    "Error: Can't send `Connection::Updated` event to server receiver"
                                );
                            }

                            offset += packet_size;
                        }

                        buff_offset = buff_readable_size - offset;
                    }
                    Err(err) => {
                        error!(peer=%peer, "Connection closed (I/O ERROR): {err:?}");
                        if tx_server
                            .send(MpscData(id, Connection::Disconnected))
                            .await
                            .is_err()
                        {
                            warn!(peer=%peer, "Can't send `Connection::Disconnected` event to server receiver");
                        }
                        break;
                    }
                };
            }
        });
    }

    /// Creates new client and sending its to `tx` channel.
    /// Runs separate thread that listening new incoming data.
    pub fn new(stream: TcpStream, tx: mpsc::Sender<MpscData>) -> Self {
        let id = ::rand::random();
        let (tx_client, rx_server) = mpsc::channel::<Vec<u8>>(1000);

        if let Err(err) = stream.set_nodelay(true) {
            warn!(client_id = id, "failed to enable TCP_NODELAY: {err:?}");
        }

        let client = Self {
            protocol: 0,
            id,
            connection: Connection::Connected,
            tx_server: tx,
            tx_client,
        };

        client.event_loop(stream, rx_server);
        client
    }
}

#[derive(thiserror::Error, Debug)]
enum AuthError {
    #[error("Connection closed by client")]
    ClosedByClient,
    #[error("Handshake: unexpected request header")]
    HsUnexpectedRequestHeader,
    #[error("Handshake: unexpected protocol version, expected one of: {0:?}, given: {1}")]
    HsUnexpectedProtocolVersion(&'static [u8], u8),
    #[error("Handshake response fault")]
    HsResponse,
    #[error("Handshake: unexpected request header (zero-terminate symbol is missed)")]
    HsZeroTerminated,
    #[error("Connection fault")]
    Connection,
}

async fn auth(stream: &mut TcpStream) -> Result<u8, AuthError> {
    use AuthError::*;

    const PROTOCOL_VERSION: u8 = 3;
    const HS_TOTAL_LEN: usize = HS_IN.len() + 2; // magic + '\0' + version

    let mut received = Vec::with_capacity(HS_TOTAL_LEN);
    let mut buff = [0u8; 64];

    loop {
        let n = match stream.read(&mut buff).await {
            Ok(0) => Err(ClosedByClient)?,
            Ok(n) => n,
            Err(_e) => Err(Connection)?,
        };

        received.extend_from_slice(&buff[..n]);

        let prefix_len = received.len().min(HS_IN.len());
        if received[..prefix_len] != HS_IN[..prefix_len] {
            Err(HsUnexpectedRequestHeader)?
        }

        if received.len() < HS_IN.len() {
            continue;
        }

        if received.len() > HS_IN.len() && received[HS_IN.len()] != 0 {
            Err(HsZeroTerminated)?
        }

        if received.len() < HS_TOTAL_LEN {
            continue;
        }

        let protocol_version = received[HS_IN.len() + 1];

        if protocol_version != PROTOCOL_VERSION {
            Err(HsUnexpectedProtocolVersion(
                &[PROTOCOL_VERSION],
                protocol_version,
            ))?
        }

        let send = HS_OUT
            .iter()
            .chain(&[0u8, protocol_version])
            .copied()
            .collect::<Vec<_>>();

        if let Err(_e) = stream.write_all(&send).await {
            Err(HsResponse)?
        }

        return Ok(protocol_version);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ::tokio::sync::mpsc;

    use super::*;

    fn test_client(queue_size: usize) -> (Client, mpsc::Receiver<Vec<u8>>) {
        let (tx_server, _rx_server) = mpsc::channel::<MpscData>(1);
        let (tx_client, rx_client) = mpsc::channel::<Vec<u8>>(queue_size);

        (
            Client {
                id: 1,
                connection: Connection::Connected,
                protocol: 3,
                tx_server,
                tx_client,
            },
            rx_client,
        )
    }

    #[test]
    fn lossy_realtime_packet_is_dropped_when_queue_is_full() {
        let (client, mut rx_client) = test_client(1);
        let first = Packet::new(Action::UPDATE_OBJECT, &[1]);
        let second = Packet::new(Action::UPDATE_OBJECT, &[2]);

        client.send(&first);
        client.send(&second);

        assert_eq!(rx_client.try_recv().unwrap(), first.as_bytes());
        assert!(rx_client.try_recv().is_err());
    }

    #[tokio::test]
    async fn reliable_packet_waits_when_queue_is_full() {
        let (client, mut rx_client) = test_client(1);
        let first = Packet::new(Action::PLAYERS_DATA, &[1]);
        let second = Packet::new(Action::DELETE_OBJECT, &[2]);

        client.send(&first);
        client.send(&second);

        assert_eq!(rx_client.recv().await.unwrap(), first.as_bytes());

        let second_bytes = ::tokio::time::timeout(Duration::from_secs(1), rx_client.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_bytes, second.as_bytes());
    }
}
