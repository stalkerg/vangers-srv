use std::{cell::BorrowError, collections::HashMap};

use ::tracing::info;

use crate::Server;
use crate::client::ClientID;
use crate::protocol::{Action, Packet};
use crate::vanject::DecodedVanjectId;

use super::{OnUpdateError, OnUpdateOk};

fn should_delete_on_leave_world(v: &crate::vanject::Vanject, player_bind_id: u8) -> bool {
    let player_station_id = player_bind_id as i32;

    // Original C++ server clears two different groups on world leave:
    //
    // 1. the leaving player's inventory list;
    // 2. private objects whose network id belongs to this player/station.
    //
    // A vanject network station is not the same thing as current ownership: an item
    // may keep the creator's/station's NetID after another player has picked it up.
    // Deleting every non-static object by station would therefore erase items from
    // another player's trunk when the original station dies or leaves the world.
    let is_players_inventory_object =
        v.is_non_global() && v.is_players() && v.player_bind_id == player_bind_id;
    let is_private_station_object = v.get_station() == player_station_id && v.is_private();

    is_players_inventory_object || is_private_station_object
}

#[derive(Debug, ::thiserror::Error)]
pub enum LeaveWorldError {
    #[error("player with client_id `{0}` not found")]
    PlayerNotFound(ClientID),
    #[error("player with client_id `{0}` not bind")]
    PlayerNotBind(ClientID),
    #[error("player with client_id `{0}` is out of all worlds")]
    WorldEmpty(ClientID),
    #[error("cannot get player's world (client_id `{0}`): {1}")]
    BorrowWorld(ClientID, BorrowError),
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_LeaveWorld {
    fn leave_world(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_LeaveWorld for Server {
    #[tracing::instrument(skip_all)]
    fn leave_world(
        &mut self,
        _: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let game = match self.get_mut_game_by_clientid(client_id) {
            Some(game) => game,
            None => return Err(LeaveWorldError::PlayerNotFound(client_id).into()),
        };

        let player = game.get_mut_player(client_id).unwrap();
        let player_bind_id = match player.bind {
            Some(bind) => bind.id(),
            None => return Err(LeaveWorldError::PlayerNotBind(client_id).into()),
        };

        let world_id = match player.world {
            Some(ref world) => match world.try_borrow() {
                Ok(w) => w.id,
                Err(e) => return Err(LeaveWorldError::BorrowWorld(client_id, e).into()),
            },
            None => return Err(LeaveWorldError::WorldEmpty(client_id).into()),
        };
        player.world = None;

        let delete = game
            .vanjects
            .iter()
            .filter(|(_, v)| should_delete_on_leave_world(v, player_bind_id))
            .map(|(&id, v)| {
                let decoded = DecodedVanjectId::new(id);
                info!(
                    action = "LEAVE_WORLD auto-delete",
                    id = decoded.id,
                    id_hex = %format_args!("0x{:08X}", decoded.id as u32),
                    station = decoded.station,
                    world = decoded.world,
                    type_name = decoded.type_name(),
                    type_id = decoded.type_id,
                    counter = decoded.counter,
                    global = decoded.global,
                    private = decoded.private,
                    players_object = decoded.players_object,
                    static_object = decoded.static_object,
                    stored_owner = v.player_bind_id,
                    packet_sender = client_id,
                    packet_world = world_id,
                    stored_world = v.get_world(),
                    decision = "auto_deleted_on_leave_world",
                    "object lifecycle"
                );
                (
                    id,
                    Packet::new(
                        Action::DELETE_OBJECT,
                        &std::iter::empty()
                            .chain(&id.to_le_bytes())
                            .chain(&[player_bind_id])
                            .chain(&v.time.to_le_bytes())
                            .copied()
                            .collect::<Vec<_>>(),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();

        let preserved_count = game
            .vanjects
            .iter()
            .filter(|(id, v)| {
                let decoded = DecodedVanjectId::new(**id);
                decoded.station == player_bind_id as i32
                    && v.is_non_global()
                    && v.is_non_static()
                    && !delete.contains_key(id)
            })
            .count();

        for (&id, v) in game.vanjects.iter() {
            let decoded = DecodedVanjectId::new(id);
            if decoded.station == player_bind_id as i32
                && v.is_non_global()
                && v.is_non_static()
                && !delete.contains_key(&id)
            {
                info!(
                    action = "LEAVE_WORLD preserve",
                    id = decoded.id,
                    id_hex = %format_args!("0x{:08X}", decoded.id as u32),
                    station = decoded.station,
                    world = decoded.world,
                    type_name = decoded.type_name(),
                    type_id = decoded.type_id,
                    counter = decoded.counter,
                    global = decoded.global,
                    private = decoded.private,
                    players_object = decoded.players_object,
                    static_object = decoded.static_object,
                    stored_owner = v.player_bind_id,
                    packet_sender = client_id,
                    packet_world = world_id,
                    stored_world = v.get_world(),
                    decision = if v.player_bind_id != player_bind_id {
                        "preserved_transferred_inventory"
                    } else {
                        "preserved_world_object"
                    },
                    "object lifecycle"
                );
            }
        }

        let delete_count = delete.len();
        // The original C++ server clears the leaving player's server-side
        // visibility/queues, but it does not send HIDE_OBJECT for every object
        // in the world. The client disables transferring immediately after
        // LEAVE_WORLD, so such packets become an ignored reliable burst.
        let skipped_legacy_hide_count = game
            .vanjects
            .iter()
            .filter(|(_, v)| v.is_non_global() && v.get_world() == world_id as i32)
            .count();

        info!(
            action = "LEAVE_WORLD summary",
            packet_sender = client_id,
            player_bind_id,
            world_id,
            delete_objects = delete_count,
            hide_objects = 0usize,
            skipped_legacy_hide_objects = skipped_legacy_hide_count,
            preserved_objects = preserved_count,
            notify_game_delete_packets = delete_count,
            notify_player_hide_packets = 0usize,
            notify_game_players_world_packets = 1u8,
            decision = "world_switch_summary",
            "world switch summary"
        );

        game.vanjects.retain(|id, _| !delete.contains_key(id));

        for (_, packet) in delete {
            self.notify_game(client_id, &packet);
        }

        // That sends by github server, but it seems to may be safety removed at all
        self.notify_game(
            client_id,
            &Packet::new(Action::PLAYERS_WORLD, &[player_bind_id, 0u8]),
        );

        Ok(OnUpdateOk::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Game, World};
    use crate::player::Player;
    use crate::vanject::{NID, Vanject};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn make_id(station: i32, world: i32, nid: i32, counter: i32) -> i32 {
        (station << 26) | (world << 22) | nid | counter
    }

    fn make_vanject(id: i32, player_bind_id: u8) -> Vanject {
        let mut vanject = Vanject::create_from_slice(
            &std::iter::empty()
                .chain(&id.to_le_bytes())
                .chain(&6i32.to_le_bytes())
                .chain(&10i16.to_le_bytes())
                .chain(&20i16.to_le_bytes())
                .chain(&15i16.to_le_bytes())
                .chain(&[1, 2, 3])
                .copied()
                .collect::<Vec<_>>(),
        )
        .unwrap();
        vanject.player_bind_id = player_bind_id;
        vanject
    }

    #[test]
    fn leave_world_removes_player_owned_non_private_objects_too() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(client_id, &world.borrow());

        let mut slot = Vanject::create_from_slice(&[
            1, 0, 2, 4, // player-owned SLOT object, non-private but non-global
            6, 0, 0, 0, // time
            10, 0, // x
            20, 0, // y
            15, 0, // radius
            1, 2, 3,
        ])
        .unwrap();
        slot.player_bind_id = 1;
        let slot_id = slot.id;
        game.vanjects.insert(slot_id, slot);

        srv.games.insert(1, game);
        srv.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), client_id)
            .unwrap();

        let game = srv.games.get(&1).unwrap();
        assert!(!game.vanjects.contains_key(&slot_id));
    }

    #[test]
    fn leave_world_preserves_inventory_now_owned_by_another_player() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let leaving_client_id: ClientID = 11;
        let other_client_id: ClientID = 22;
        game.attach_player(Player::new(leaving_client_id));
        game.attach_player(Player::new(other_client_id));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(leaving_client_id, &world.borrow());

        let station = 1;
        let world_id = 1;
        let slot_id = make_id(station, world_id, NID::SLOT, 3);

        // The network id still belongs to station 1, but the server-side current
        // inventory owner is player 2. This is the "item in another player's
        // trunk" case: leaving player 1 must not delete it.
        game.vanjects.insert(slot_id, make_vanject(slot_id, 2));

        srv.games.insert(1, game);
        srv.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), leaving_client_id)
            .unwrap();

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&slot_id));
    }

    #[test]
    fn leave_world_preserves_dropped_world_stuff_created_by_player() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(client_id, &world.borrow());

        let stuff_id = make_id(1, 1, NID::STUFF, 4);
        game.vanjects.insert(stuff_id, make_vanject(stuff_id, 1));

        srv.games.insert(1, game);
        srv.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), client_id)
            .unwrap();

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&stuff_id));
    }

    #[test]
    fn leave_world_removes_private_station_objects() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(client_id, &world.borrow());

        let private_id = make_id(1, 1, NID::VANGER, 5);
        game.vanjects
            .insert(private_id, make_vanject(private_id, 2));

        srv.games.insert(1, game);
        srv.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), client_id)
            .unwrap();

        let game = srv.games.get(&1).unwrap();
        assert!(!game.vanjects.contains_key(&private_id));
    }

    #[test]
    fn leave_world_preserves_static_world_objects() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(client_id, &world.borrow());

        let station = 1 << 26;
        let world_bits = 1 << 22;
        let sensor_id = station | world_bits | NID::SENSOR | 1;
        let tnt_id = station | world_bits | NID::TNT | 2;
        let slot_id = station | world_bits | NID::SLOT | 3;

        game.vanjects.insert(sensor_id, make_vanject(sensor_id, 1));
        game.vanjects.insert(tnt_id, make_vanject(tnt_id, 1));
        game.vanjects.insert(slot_id, make_vanject(slot_id, 1));

        srv.games.insert(1, game);
        srv.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), client_id)
            .unwrap();

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&sensor_id));
        assert!(game.vanjects.contains_key(&tnt_id));
        assert!(!game.vanjects.contains_key(&slot_id));
    }

    #[tokio::test]
    async fn leave_world_does_not_send_world_hide_burst_to_leaving_player() {
        use crate::client::{Client, Connection, MpscData};
        use ::tokio::io::{AsyncReadExt, AsyncWriteExt};
        use ::tokio::net::{TcpListener, TcpStream};
        use ::tokio::sync::mpsc;
        use ::tokio::time::{Duration, timeout};

        const HS_IN: &[u8] = b"Vivat Sicher, Rock'n'Roll forever!!!";
        const HS_OUT: &[u8] = b"Enter, my son, please...";
        const PROTOCOL_VERSION: u8 = crate::client::PROTOCOL_VERSION;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_connect = ::tokio::spawn(async move { TcpStream::connect(addr).await });
        let (server_stream, _) = listener.accept().await.unwrap();
        let mut client_stream = client_connect.await.unwrap().unwrap();

        let (tx_server, mut rx_server) = mpsc::channel::<MpscData>(8);
        let client = Client::new(server_stream, tx_server);
        let client_id = client.id;

        client_stream.write_all(HS_IN).await.unwrap();
        client_stream
            .write_all(&[0, PROTOCOL_VERSION])
            .await
            .unwrap();

        let mut handshake = vec![0u8; HS_OUT.len() + 2];
        client_stream.read_exact(&mut handshake).await.unwrap();
        assert_eq!(
            handshake,
            HS_OUT
                .iter()
                .chain(&[0, PROTOCOL_VERSION])
                .copied()
                .collect::<Vec<_>>()
        );

        let auth = timeout(Duration::from_secs(1), rx_server.recv())
            .await
            .unwrap()
            .unwrap();
        match auth {
            MpscData(id, Connection::Authenticated(protocol)) => {
                assert_eq!(id, client_id);
                assert_eq!(protocol, PROTOCOL_VERSION);
            }
            _ => panic!("client must authenticate before LEAVE_WORLD test"),
        }

        let mut srv = Server::new(Default::default());
        srv.clients.push(client);

        let mut game = Game::new(1);
        game.attach_player(Player::new(client_id));
        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(client_id, &world.borrow());

        let station = 1 << 26;
        let world_bits = 1 << 22;
        let sensor_id = station | world_bits | NID::SENSOR | 1;
        let tnt_id = station | world_bits | NID::TNT | 2;
        game.vanjects.insert(sensor_id, make_vanject(sensor_id, 1));
        game.vanjects.insert(tnt_id, make_vanject(tnt_id, 1));

        srv.games.insert(1, game);
        srv.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), client_id)
            .unwrap();

        let mut header = [0u8; 2];
        let read = timeout(
            Duration::from_millis(150),
            client_stream.read_exact(&mut header),
        )
        .await;
        assert!(
            read.is_err(),
            "LEAVE_WORLD must not send HIDE_OBJECT burst to the leaving client"
        );
    }
}
