use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};

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
type LatestObjectUpdatesSlot = Arc<Mutex<LatestObjectUpdates>>;

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
    latest_object_updates: LatestObjectUpdatesSlot,
    latest_object_updates_notify: Arc<Notify>,
}

impl Client {
    pub fn send(&self, packet: &Packet) {
        match packet.action {
            Action::SERVER_TIME => self.send_latest_server_time(packet),
            _ => self.send_reliable(packet),
        }
    }

    pub fn send_reliable(&self, packet: &Packet) {
        let action = packet.action;
        if matches!(action, Action::DELETE_OBJECT | Action::HIDE_OBJECT) {
            if let Some(object_id) = packet_object_id(packet) {
                let removed = remove_latest_object_update(&self.latest_object_updates, object_id);
                if removed {
                    trace!(
                        client_id = self.id,
                        action = ?action,
                        object_id,
                        object_id_hex = %format_args!("0x{:08X}", object_id as u32),
                        decision = "dropped_pending_realtime_update",
                        "dropped pending realtime UPDATE_OBJECT before lifecycle packet"
                    );
                }
            }
        }

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

    pub fn send_realtime_update(&self, packet: &Packet) {
        if packet.action != Action::UPDATE_OBJECT {
            self.send(packet);
            return;
        }

        match packet_object_id(packet) {
            Some(object_id) => {
                let replaced = replace_latest_object_update(
                    &self.latest_object_updates,
                    object_id,
                    packet.as_bytes(),
                );
                if replaced {
                    trace!(
                        client_id = self.id,
                        action = ?packet.action,
                        object_id,
                        object_id_hex = %format_args!("0x{:08X}", object_id as u32),
                        decision = "realtime_update_replaced_pending",
                        "replaced pending realtime UPDATE_OBJECT with a newer one"
                    );
                }
                self.latest_object_updates_notify.notify_one();
            }
            None => {
                warn!(
                    client_id = self.id,
                    action = ?packet.action,
                    decision = "malformed_realtime_update_fallback_reliable",
                    "UPDATE_OBJECT without object id; sending through reliable queue"
                );
                self.send_reliable(packet);
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

    fn event_loop(&self, mut stream: TcpStream, rx_server: Receiver<Vec<u8>>) {
        let tx_server = self.tx_server.clone();
        let latest_server_time = Arc::clone(&self.latest_server_time);
        let latest_server_time_notify = Arc::clone(&self.latest_server_time_notify);
        let latest_object_updates = Arc::clone(&self.latest_object_updates);
        let latest_object_updates_notify = Arc::clone(&self.latest_object_updates_notify);
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
                latest_object_updates,
                latest_object_updates_notify,
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
        let latest_object_updates = Arc::new(Mutex::new(LatestObjectUpdates::default()));
        let latest_object_updates_notify = Arc::new(Notify::new());

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
            latest_object_updates,
            latest_object_updates_notify,
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

#[derive(Default)]
struct LatestObjectUpdates {
    order: VecDeque<i32>,
    packets: HashMap<i32, Vec<u8>>,
}

impl LatestObjectUpdates {
    fn replace(&mut self, object_id: i32, data: Vec<u8>) -> bool {
        let replaced = self.packets.insert(object_id, data).is_some();
        if !replaced {
            self.order.push_back(object_id);
        }
        replaced
    }

    fn take_next(&mut self) -> Option<Vec<u8>> {
        while let Some(object_id) = self.order.pop_front() {
            if let Some(data) = self.packets.remove(&object_id) {
                return Some(data);
            }
        }
        None
    }

    fn remove(&mut self, object_id: i32) -> bool {
        let removed = self.packets.remove(&object_id).is_some();
        if removed {
            self.order.retain(|&queued_id| queued_id != object_id);
        }
        removed
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.packets.len()
    }
}

fn replace_latest_object_update(
    slot: &LatestObjectUpdatesSlot,
    object_id: i32,
    data: Vec<u8>,
) -> bool {
    let mut pending = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.replace(object_id, data)
}

fn take_next_latest_object_update(slot: &LatestObjectUpdatesSlot) -> Option<Vec<u8>> {
    let mut pending = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.take_next()
}

fn remove_latest_object_update(slot: &LatestObjectUpdatesSlot, object_id: i32) -> bool {
    let mut pending = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.remove(object_id)
}

fn packet_object_id(packet: &Packet) -> Option<i32> {
    let id = packet.data.get(0..4)?;
    Some(i32::from_le_bytes([id[0], id[1], id[2], id[3]]))
}

async fn writer_loop<W>(
    mut sw: W,
    mut rx_server: Receiver<Vec<u8>>,
    latest_server_time: LatestServerTimeSlot,
    latest_server_time_notify: Arc<Notify>,
    latest_object_updates: LatestObjectUpdatesSlot,
    latest_object_updates_notify: Arc<Notify>,
    peer: String,
) where
    W: AsyncWrite + Unpin,
{
    let mut rx_server_closed = false;

    loop {
        if let Some(data) = take_latest_server_time(&latest_server_time) {
            if !write_client_packet(&mut sw, &data, &peer).await {
                break;
            }
            continue;
        }

        if !rx_server_closed {
            match rx_server.try_recv() {
                Ok(data) => {
                    if !write_client_packet(&mut sw, &data, &peer).await {
                        break;
                    }
                    continue;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    rx_server_closed = true;
                }
            }
        }

        if let Some(data) = take_next_latest_object_update(&latest_object_updates) {
            if !write_client_packet(&mut sw, &data, &peer).await {
                break;
            }
            continue;
        }

        if rx_server_closed {
            break;
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
                    None => {
                        rx_server_closed = true;
                    }
                }
            }
            _ = latest_object_updates_notify.notified() => {
                continue;
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
    use std::collections::HashSet;
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
                latest_object_updates: Arc::new(Mutex::new(LatestObjectUpdates::default())),
                latest_object_updates_notify: Arc::new(Notify::new()),
            },
            rx_client,
        )
    }

    fn make_update_packet(object_id: i32, time: i32) -> Packet {
        Packet::new(
            Action::UPDATE_OBJECT,
            &std::iter::empty()
                .chain(&object_id.to_le_bytes())
                .chain(&time.to_le_bytes())
                .chain(&10i16.to_le_bytes())
                .chain(&20i16.to_le_bytes())
                .chain(&[1, 2, 3])
                .copied()
                .collect::<Vec<_>>(),
        )
    }

    fn make_object_lifecycle_packet(action: Action, object_id: i32) -> Packet {
        Packet::new(
            action,
            &std::iter::empty()
                .chain(&object_id.to_le_bytes())
                .chain(&1i32.to_le_bytes())
                .copied()
                .collect::<Vec<_>>(),
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
    fn update_object_uses_reliable_fifo_by_default() {
        let (client, mut rx_client) = test_client(1);
        let update = make_update_packet(1, 10);

        client.send(&update);

        assert_eq!(rx_client.try_recv().unwrap(), update.as_bytes());
        assert!(rx_client.try_recv().is_err());
    }

    #[test]
    fn realtime_update_uses_latest_object_slot_instead_of_fifo_queue() {
        let (client, mut rx_client) = test_client(10);
        let update = make_update_packet(1, 10);

        client.send_realtime_update(&update);

        assert!(rx_client.try_recv().is_err());
        assert_eq!(
            take_next_latest_object_update(&client.latest_object_updates).unwrap(),
            update.as_bytes()
        );
        assert!(take_next_latest_object_update(&client.latest_object_updates).is_none());
    }

    #[test]
    fn newer_realtime_update_replaces_pending_update_for_same_object() {
        let (client, mut rx_client) = test_client(10);
        let old = make_update_packet(1, 10);
        let new = make_update_packet(1, 20);

        client.send_realtime_update(&old);
        client.send_realtime_update(&new);

        assert!(rx_client.try_recv().is_err());
        assert_eq!(
            take_next_latest_object_update(&client.latest_object_updates).unwrap(),
            new.as_bytes()
        );
        assert!(take_next_latest_object_update(&client.latest_object_updates).is_none());
    }

    #[test]
    fn different_realtime_objects_are_kept_independently() {
        let (client, mut rx_client) = test_client(10);
        let first = make_update_packet(1, 10);
        let second = make_update_packet(2, 20);
        let third = make_update_packet(3, 30);

        client.send_realtime_update(&first);
        client.send_realtime_update(&second);
        client.send_realtime_update(&third);

        assert!(rx_client.try_recv().is_err());
        assert_eq!(
            client.latest_object_updates.lock().unwrap().len(),
            3,
            "three different object ids must remain independently pending"
        );
        assert_eq!(
            take_next_latest_object_update(&client.latest_object_updates).unwrap(),
            first.as_bytes()
        );
        assert_eq!(
            take_next_latest_object_update(&client.latest_object_updates).unwrap(),
            second.as_bytes()
        );
        assert_eq!(
            take_next_latest_object_update(&client.latest_object_updates).unwrap(),
            third.as_bytes()
        );
    }

    #[test]
    fn replacement_does_not_duplicate_object_queue_key() {
        let (client, mut rx_client) = test_client(10);
        let first = make_update_packet(1, 10);
        let second = make_update_packet(1, 20);
        let third = make_update_packet(1, 30);

        client.send_realtime_update(&first);
        client.send_realtime_update(&second);
        client.send_realtime_update(&third);

        assert!(rx_client.try_recv().is_err());
        assert_eq!(
            client.latest_object_updates.lock().unwrap().len(),
            1,
            "replacing one object must not enqueue duplicate ids"
        );
        assert_eq!(
            take_next_latest_object_update(&client.latest_object_updates).unwrap(),
            third.as_bytes()
        );
        assert!(take_next_latest_object_update(&client.latest_object_updates).is_none());
    }

    #[test]
    fn delete_object_drops_pending_realtime_update_for_same_object() {
        let (client, mut rx_client) = test_client(10);
        let update = make_update_packet(1, 10);
        let delete = make_object_lifecycle_packet(Action::DELETE_OBJECT, 1);

        client.send_realtime_update(&update);
        client.send(&delete);

        assert_eq!(rx_client.try_recv().unwrap(), delete.as_bytes());
        assert!(take_next_latest_object_update(&client.latest_object_updates).is_none());
    }

    #[test]
    fn hide_object_drops_pending_realtime_update_for_same_object() {
        let (client, mut rx_client) = test_client(10);
        let update = make_update_packet(1, 10);
        let hide = make_object_lifecycle_packet(Action::HIDE_OBJECT, 1);

        client.send_realtime_update(&update);
        client.send(&hide);

        assert_eq!(rx_client.try_recv().unwrap(), hide.as_bytes());
        assert!(take_next_latest_object_update(&client.latest_object_updates).is_none());
    }

    #[test]
    fn malformed_realtime_update_falls_back_to_reliable_fifo() {
        let (client, mut rx_client) = test_client(10);
        let malformed = Packet::new(Action::UPDATE_OBJECT, &[1, 2, 3]);

        client.send_realtime_update(&malformed);

        assert_eq!(rx_client.try_recv().unwrap(), malformed.as_bytes());
        assert!(take_next_latest_object_update(&client.latest_object_updates).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_realtime_updates_keep_one_pending_packet_per_object() {
        let (client, mut rx_client) = test_client(10);
        let client = Arc::new(client);
        let mut tasks = Vec::new();

        for i in 0..200 {
            let client = Arc::clone(&client);
            tasks.push(::tokio::spawn(async move {
                let object_id = 1 + (i % 8);
                let update = make_update_packet(object_id, i);
                client.send_realtime_update(&update);
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        assert!(rx_client.try_recv().is_err());
        assert_eq!(client.latest_object_updates.lock().unwrap().len(), 8);

        let mut seen = HashSet::new();
        while let Some(data) = take_next_latest_object_update(&client.latest_object_updates) {
            let packet = Packet::from_slice(&data);
            let object_id = packet_object_id(&packet).unwrap();
            assert!(
                (1..=8).contains(&object_id),
                "unexpected object_id={object_id}"
            );
            assert!(
                seen.insert(object_id),
                "object_id={object_id} appeared more than once"
            );
        }
        assert_eq!(seen.len(), 8);
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
        let latest_object_updates = Arc::new(Mutex::new(LatestObjectUpdates::default()));
        let latest_object_updates_notify = Arc::new(Notify::new());

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
            Arc::clone(&latest_object_updates),
            Arc::clone(&latest_object_updates_notify),
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
        let latest_object_updates = Arc::new(Mutex::new(LatestObjectUpdates::default()));
        let latest_object_updates_notify = Arc::new(Notify::new());

        let (mut reader, writer) = duplex(1024);
        let writer_task = ::tokio::spawn(writer_loop(
            writer,
            rx_server,
            Arc::clone(&latest_server_time),
            Arc::clone(&latest_server_time_notify),
            Arc::clone(&latest_object_updates),
            Arc::clone(&latest_object_updates_notify),
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

    #[tokio::test]
    async fn writer_sends_fifo_before_realtime_object_update() {
        let (tx_client, rx_server) = mpsc::channel::<Vec<u8>>(10);
        let latest_server_time = Arc::new(Mutex::new(None));
        let latest_server_time_notify = Arc::new(Notify::new());
        let latest_object_updates = Arc::new(Mutex::new(LatestObjectUpdates::default()));
        let latest_object_updates_notify = Arc::new(Notify::new());

        let fifo = Packet::new(Action::PLAYERS_DATA, &[1]);
        let update = make_update_packet(1, 10);
        tx_client.try_send(fifo.as_bytes()).unwrap();
        replace_latest_object_update(&latest_object_updates, 1, update.as_bytes());

        let (mut reader, writer) = duplex(1024);
        let writer_task = ::tokio::spawn(writer_loop(
            writer,
            rx_server,
            Arc::clone(&latest_server_time),
            Arc::clone(&latest_server_time_notify),
            Arc::clone(&latest_object_updates),
            Arc::clone(&latest_object_updates_notify),
            "test-peer".to_string(),
        ));

        assert_eq!(read_packet(&mut reader).await, fifo.as_bytes());
        assert_eq!(read_packet(&mut reader).await, update.as_bytes());

        drop(tx_client);
        writer_task.abort();
    }

    #[tokio::test]
    async fn writer_observes_realtime_object_notify_without_fifo_activity() {
        let (_tx_client, rx_server) = mpsc::channel::<Vec<u8>>(10);
        let latest_server_time = Arc::new(Mutex::new(None));
        let latest_server_time_notify = Arc::new(Notify::new());
        let latest_object_updates = Arc::new(Mutex::new(LatestObjectUpdates::default()));
        let latest_object_updates_notify = Arc::new(Notify::new());

        let (mut reader, writer) = duplex(1024);
        let writer_task = ::tokio::spawn(writer_loop(
            writer,
            rx_server,
            Arc::clone(&latest_server_time),
            Arc::clone(&latest_server_time_notify),
            Arc::clone(&latest_object_updates),
            Arc::clone(&latest_object_updates_notify),
            "test-peer".to_string(),
        ));

        let update = make_update_packet(1, 10);
        replace_latest_object_update(&latest_object_updates, 1, update.as_bytes());
        latest_object_updates_notify.notify_one();

        let packet = ::tokio::time::timeout(Duration::from_secs(1), read_packet(&mut reader))
            .await
            .unwrap();
        assert_eq!(packet, update.as_bytes());

        writer_task.abort();
    }
}
