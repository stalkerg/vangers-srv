use std::convert::TryFrom;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ::tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use ::tokio::net::TcpStream;
use ::tokio::sync::{
    Notify,
    mpsc::{self, Receiver, error::TrySendError},
};
use ::tracing::{error, info, trace, warn};

use super::protocol::*;

const HS_IN: &[u8] = b"Vivat Sicher, Rock'n'Roll forever!!!";
const HS_OUT: &[u8] = b"Enter, my son, please...";

pub type ClientID = usize;
type LatestServerTimeSlot = Arc<Mutex<Option<Vec<u8>>>>;

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
    latest_server_time: LatestServerTimeSlot,
    latest_server_time_notify: Arc<Notify>,
    lossy_drop_last_log_ms: AtomicU64,
    lossy_drop_suppressed: AtomicU64,
}

impl Client {
    pub fn send(&self, packet: &Packet) {
        match packet.action {
            Action::SERVER_TIME => self.send_latest_server_time(packet),
            _ if packet.action.is_lossy_realtime() => self.send_lossy(packet),
            _ => self.send_reliable(packet),
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
                    decision = "reliable_fallback_send_spawned",
                    "client outgoing queue full; reliable fallback send spawned"
                );
                ::tokio::spawn(async move {
                    if tx_client.send(data).await.is_err() {
                        warn!(
                            client_id,
                            action = ?action,
                            decision = "reliable_send_failed",
                            "client outbound queue is closed; reliable send failed"
                        );
                    }
                });
            }
            Err(TrySendError::Closed(_)) => {
                warn!(
                    client_id = self.id,
                    action = ?action,
                    decision = "reliable_send_failed",
                    "client outbound queue is closed; reliable send failed"
                );
            }
        }
    }

    pub fn send_lossy(&self, packet: &Packet) {
        let action = packet.action;

        match self.tx_client.try_send(packet.as_bytes()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.log_lossy_drop(action, "client outgoing queue full");
            }
            Err(TrySendError::Closed(_)) => {
                self.log_lossy_drop(action, "client outbound queue is closed");
            }
        }
    }

    fn send_latest_server_time(&self, packet: &Packet) {
        let replaced = replace_latest_server_time(&self.latest_server_time, packet.as_bytes());
        if replaced {
            trace!(
                client_id = self.id,
                action = ?packet.action,
                decision = "server_time_replaced_pending",
                "replaced pending SERVER_TIME response with a newer one"
            );
        }
        self.latest_server_time_notify.notify_one();
    }

    fn log_lossy_drop(&self, action: Action, reason: &'static str) {
        const LOG_INTERVAL_MS: u64 = 5_000;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let last = self.lossy_drop_last_log_ms.load(Ordering::Relaxed);

        if last != 0 && now_ms.saturating_sub(last) < LOG_INTERVAL_MS {
            self.lossy_drop_suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }

        if self
            .lossy_drop_last_log_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            let suppressed = self.lossy_drop_suppressed.swap(0, Ordering::Relaxed);
            warn!(
                client_id = self.id,
                action = ?action,
                decision = if action == Action::UPDATE_OBJECT {
                    "lossy UPDATE_OBJECT dropped"
                } else {
                    "lossy SERVER_TIME dropped"
                },
                suppressed_drops = suppressed,
                "{reason}; dropping lossy realtime packet"
            );
        } else {
            self.lossy_drop_suppressed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn event_loop(&self, mut stream: TcpStream, rx_server: Receiver<Vec<u8>>) {
        let tx_server = self.tx_server.clone();
        let latest_server_time = Arc::clone(&self.latest_server_time);
        let latest_server_time_notify = Arc::clone(&self.latest_server_time_notify);
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

            let (mut sr, sw) = stream.into_split();

            let writer_peer = peer.clone();
            ::tokio::spawn(writer_loop(
                sw,
                rx_server,
                latest_server_time,
                latest_server_time_notify,
                writer_peer,
            ));

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
        let latest_server_time = Arc::new(Mutex::new(None));
        let latest_server_time_notify = Arc::new(Notify::new());

        if let Err(err) = stream.set_nodelay(true) {
            warn!(client_id = id, "failed to enable TCP_NODELAY: {err:?}");
        } else {
            info!(client_id = id, "TCP_NODELAY enabled");
        }

        let client = Self {
            protocol: 0,
            id,
            connection: Connection::Connected,
            tx_server: tx,
            tx_client,
            latest_server_time,
            latest_server_time_notify,
            lossy_drop_last_log_ms: AtomicU64::new(0),
            lossy_drop_suppressed: AtomicU64::new(0),
        };

        client.event_loop(stream, rx_server);
        client
    }
}

fn replace_latest_server_time(slot: &LatestServerTimeSlot, data: Vec<u8>) -> bool {
    let mut pending = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.replace(data).is_some()
}

fn take_latest_server_time(slot: &LatestServerTimeSlot) -> Option<Vec<u8>> {
    let mut pending = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.take()
}

async fn writer_loop<W>(
    mut sw: W,
    mut rx_server: Receiver<Vec<u8>>,
    latest_server_time: LatestServerTimeSlot,
    latest_server_time_notify: Arc<Notify>,
    peer: String,
) where
    W: AsyncWrite + Unpin,
{
    loop {
        if let Some(data) = take_latest_server_time(&latest_server_time) {
            if !write_client_packet(&mut sw, &data, &peer).await {
                break;
            }
            continue;
        }

        ::tokio::select! {
            biased;
            _ = latest_server_time_notify.notified() => {
                continue;
            }
            data = rx_server.recv() => {
                match data {
                    Some(data) => {
                        if !write_client_packet(&mut sw, &data, &peer).await {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

async fn write_client_packet<W>(sw: &mut W, data: &[u8], peer: &str) -> bool
where
    W: AsyncWrite + Unpin,
{
    if let Err(err) = sw.write_all(data).await {
        error!(peer=%peer, "write_all error: {err:?}");
        return false;
    }

    true
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ::tokio::io::{AsyncRead, AsyncReadExt, duplex};
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
                latest_server_time: Arc::new(Mutex::new(None)),
                latest_server_time_notify: Arc::new(Notify::new()),
                lossy_drop_last_log_ms: AtomicU64::new(0),
                lossy_drop_suppressed: AtomicU64::new(0),
            },
            rx_client,
        )
    }

    async fn read_packet<R>(reader: &mut R) -> Vec<u8>
    where
        R: AsyncRead + Unpin,
    {
        let mut header = [0u8; 2];
        reader.read_exact(&mut header).await.unwrap();
        let body_size = ((header[0] as i16) | ((header[1] as i16) << 8)) as usize;
        let mut body = vec![0u8; body_size];
        reader.read_exact(&mut body).await.unwrap();

        let mut packet = header.to_vec();
        packet.extend_from_slice(&body);
        packet
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

    #[test]
    fn server_time_uses_latest_slot_instead_of_fifo_queue() {
        let (client, mut rx_client) = test_client(10);
        let packet = Packet::new(Action::SERVER_TIME, &[1, 2, 3, 4]);

        client.send(&packet);

        assert!(rx_client.try_recv().is_err());
        assert_eq!(
            take_latest_server_time(&client.latest_server_time).unwrap(),
            packet.as_bytes()
        );
        assert!(take_latest_server_time(&client.latest_server_time).is_none());
    }

    #[test]
    fn newer_server_time_replaces_pending_server_time() {
        let (client, mut rx_client) = test_client(10);
        let old = Packet::new(Action::SERVER_TIME, &[1, 0, 0, 0]);
        let new = Packet::new(Action::SERVER_TIME, &[2, 0, 0, 0]);

        client.send(&old);
        client.send(&new);

        assert!(rx_client.try_recv().is_err());
        assert_eq!(
            take_latest_server_time(&client.latest_server_time).unwrap(),
            new.as_bytes()
        );
        assert!(take_latest_server_time(&client.latest_server_time).is_none());
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

    #[tokio::test]
    async fn writer_sends_pending_server_time_before_fifo_packets() {
        let (tx_client, rx_server) = mpsc::channel::<Vec<u8>>(10);
        let latest_server_time = Arc::new(Mutex::new(None));
        let latest_server_time_notify = Arc::new(Notify::new());

        let fifo = Packet::new(Action::PLAYERS_DATA, &[1]);
        let server_time = Packet::new(Action::SERVER_TIME, &[2, 0, 0, 0]);
        tx_client.try_send(fifo.as_bytes()).unwrap();
        replace_latest_server_time(&latest_server_time, server_time.as_bytes());

        let (mut reader, writer) = duplex(1024);
        let writer_task = ::tokio::spawn(writer_loop(
            writer,
            rx_server,
            Arc::clone(&latest_server_time),
            Arc::clone(&latest_server_time_notify),
            "test-peer".to_string(),
        ));

        assert_eq!(read_packet(&mut reader).await, server_time.as_bytes());
        assert_eq!(read_packet(&mut reader).await, fifo.as_bytes());

        drop(tx_client);
        writer_task.abort();
    }

    #[tokio::test]
    async fn writer_observes_server_time_notify_without_fifo_activity() {
        let (_tx_client, rx_server) = mpsc::channel::<Vec<u8>>(10);
        let latest_server_time = Arc::new(Mutex::new(None));
        let latest_server_time_notify = Arc::new(Notify::new());

        let (mut reader, writer) = duplex(1024);
        let writer_task = ::tokio::spawn(writer_loop(
            writer,
            rx_server,
            Arc::clone(&latest_server_time),
            Arc::clone(&latest_server_time_notify),
            "test-peer".to_string(),
        ));

        let server_time = Packet::new(Action::SERVER_TIME, &[3, 0, 0, 0]);
        replace_latest_server_time(&latest_server_time, server_time.as_bytes());
        latest_server_time_notify.notify_one();

        let packet = ::tokio::time::timeout(Duration::from_secs(1), read_packet(&mut reader))
            .await
            .unwrap();
        assert_eq!(packet, server_time.as_bytes());

        writer_task.abort();
    }
}
