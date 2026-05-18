#![allow(clippy::collapsible_else_if)]

use ::tracing::{debug, info, warn};

use crate::Server;
use crate::client::ClientID;
use crate::protocol::{Action, NetTransportSend, Packet};
use crate::vanject::*;

use super::item_transfer::{is_item_vanject, item_has_paired_vanject, item_state_packet};
use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum CreateObjectError {
    #[error("fail read slice as vanject: [{0}]")]
    SliceToVanjectParse(VanjectError),
    #[error("player with `client_id`={0} not found")]
    PlayerNotFound(ClientID),
    #[error("player with `client_id`={0} not bind")]
    PlayerNotBind(ClientID),
    #[error(
        "item CREATE_OBJECT for `id`={0} conflicts with existing paired item `id`={1}; use ITEM_TRANSFER"
    )]
    PairedItemAlreadyExists(i32, i32),
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_CreateObject {
    fn create_object(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_CreateObject for Server {
    #[tracing::instrument(skip_all)]
    fn create_object(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let mut vanject = match Vanject::create_from_slice(&packet.data) {
            Ok(vanject) => vanject,
            Err(err) => return Err(CreateObjectError::SliceToVanjectParse(err).into()),
        };
        let decoded = DecodedVanjectId::new(vanject.id);

        let game = match self.get_mut_game_by_clientid(client_id) {
            Some(game) => game,
            None => return Err(CreateObjectError::PlayerNotFound(client_id).into()),
        };

        if let Some(existing) = game.vanjects.get(&vanject.id) {
            debug!("VANJECT with id=`{}` already exists", vanject.id);
            let existing_decoded = DecodedVanjectId::new(existing.id);
            info!(
                action = "CREATE_OBJECT",
                id = existing_decoded.id,
                id_hex = %format_args!("0x{:08X}", existing_decoded.id as u32),
                station = existing_decoded.station,
                world = existing_decoded.world,
                type_name = existing_decoded.type_name(),
                type_id = existing_decoded.type_id,
                counter = existing_decoded.counter,
                global = existing_decoded.global,
                private = existing_decoded.private,
                players_object = existing_decoded.players_object,
                static_object = existing_decoded.static_object,
                stored_owner = existing.player_bind_id,
                packet_sender = client_id,
                packet_world = decoded.world,
                stored_world = existing_decoded.world,
                decision = "duplicate CREATE_OBJECT replay_existing",
                "object lifecycle"
            );

            let mut packets = vec![Packet::new(
                Action::UPDATE_OBJECT,
                &existing.to_vangers_byte(),
            )];
            if is_item_vanject(existing) {
                packets[0] = item_state_packet(0, existing);
            }
            if existing.get_type() == NID::VANGER {
                let data = std::iter::empty()
                    .chain(&[existing.player_bind_id])
                    .chain(&existing.pos.to_vangers_byte())
                    .copied()
                    .collect::<Vec<_>>();
                packets.push(Packet::new(Action::PLAYERS_POSITION, &data));
            }

            let _ = game;
            packets
                .iter()
                .for_each(|packet| self.notify_player(client_id, packet));
        } else {
            if is_item_vanject(&vanject) {
                if let Some(paired_id) = item_has_paired_vanject(&vanject, &game.vanjects) {
                    warn!(
                        action = "CREATE_OBJECT",
                        id = decoded.id,
                        id_hex = %format_args!("0x{:08X}", decoded.id as u32),
                        station = decoded.station,
                        world = decoded.world,
                        type_name = decoded.type_name(),
                        type_id = decoded.type_id,
                        counter = decoded.counter,
                        paired_id,
                        paired_id_hex = %format_args!("0x{:08X}", paired_id as u32),
                        packet_sender = client_id,
                        packet_world = decoded.world,
                        decision = "rejected_paired_item_already_exists_use_item_transfer",
                        "object lifecycle"
                    );
                    Err(CreateObjectError::PairedItemAlreadyExists(
                        vanject.id, paired_id,
                    ))?;
                }
            }

            let player = game.get_mut_player(client_id).unwrap();
            if vanject.bind_to_player(player).is_err() {
                return Err(CreateObjectError::PlayerNotBind(client_id).into());
            }
            let player_bind_id = vanject.player_bind_id;
            info!(
                action = "CREATE_OBJECT",
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
                stored_owner = player_bind_id,
                packet_sender = client_id,
                packet_world = decoded.world,
                stored_world = decoded.world,
                decision = "accepted",
                "object lifecycle"
            );

            if vanject.get_type() == NID::VANGER {
                player.pos = vanject.pos;

                if player.set_body(&vanject.body).is_err() {
                    warn!("NID::VANGER: set body failed");
                } else {
                    let data = vanject.to_vangers_byte();
                    let answer = Packet::new(Action::UPDATE_OBJECT, &data);
                    self.notify_world(client_id, vanject.get_world() as u8, &answer, false);
                }
            } else {
                // #IF: vanject.get_type() != NID::VANGER
                let answer = if is_item_vanject(&vanject) {
                    item_state_packet(0, &vanject)
                } else {
                    let data = vanject.to_vangers_byte();
                    Packet::new(Action::UPDATE_OBJECT, &data)
                };

                if !vanject.is_players() {
                    // world->process_create;
                    if vanject.is_non_global() {
                        self.notify_world(client_id, vanject.get_world() as u8, &answer, false);
                    } else {
                        self.notify_game(client_id, &answer);
                    }
                } else {
                    if vanject.is_non_global() {
                        // world->process_create_inventory()
                        debug!(
                            "Added vanject {:?} to inventory of player_id=`{}`",
                            &vanject.id.to_le_bytes(),
                            vanject.player_bind_id
                        );
                        self.notify_world(client_id, vanject.get_world() as u8, &answer, false);
                    } else {
                        // game->process_create_globals()
                        self.notify_game(client_id, &answer);
                    }
                }
            }

            self.get_mut_game_by_clientid(client_id)
                .unwrap()
                .vanjects
                .insert(vanject.id, vanject);
        }

        Ok(OnUpdateOk::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Game, World};
    use crate::player::Player;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn make_id(station: i32, world: i32, nid: i32, counter: i32) -> i32 {
        (station << 26) | (world << 22) | nid | counter
    }

    fn make_item_create_payload(id: i32, linked_id: i32, net_owner: i32) -> Vec<u8> {
        std::iter::empty()
            .chain(&id.to_le_bytes())
            .chain(&6i32.to_le_bytes())
            .chain(&10i16.to_le_bytes())
            .chain(&20i16.to_le_bytes())
            .chain(&15i16.to_le_bytes())
            .chain(&[7u8])
            .chain(&linked_id.to_le_bytes())
            .chain(&30i16.to_le_bytes())
            .chain(&[8u8])
            .chain(&9i32.to_le_bytes())
            .chain(&10i32.to_le_bytes())
            .chain(&[0u8])
            .chain(&11i16.to_le_bytes())
            .chain(&12i16.to_le_bytes())
            .chain(&13i16.to_le_bytes())
            .chain(&net_owner.to_le_bytes())
            .copied()
            .collect()
    }

    fn make_server_with_player() -> (Server, ClientID) {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));

        let player: ClientID = 11;
        game.attach_player(Player::new(player));
        game.place_player(player, &world.borrow());

        srv.games.insert(1, game);
        (srv, player)
    }

    fn insert_item_vanject(srv: &mut Server, id: i32, owner: u8, linked_id: i32, net_owner: i32) {
        let mut vanject =
            Vanject::create_from_slice(&make_item_create_payload(id, linked_id, net_owner))
                .unwrap();
        vanject.player_bind_id = owner;
        srv.games.get_mut(&1).unwrap().vanjects.insert(id, vanject);
    }

    #[test]
    fn rejects_item_create_when_paired_item_already_exists() {
        let (mut srv, player) = make_server_with_player();
        let existing_stuff = make_id(1, 1, NID::STUFF, 91);
        let new_device = make_id(1, 1, NID::DEVICE, 91);
        insert_item_vanject(&mut srv, existing_stuff, 1, new_device, 0);

        let packet = Packet::new(
            Action::CREATE_OBJECT,
            &make_item_create_payload(new_device, existing_stuff, 1),
        );

        let err = srv.create_object(&packet, player).unwrap_err();
        match err {
            OnUpdateError::CreateObjectError(CreateObjectError::PairedItemAlreadyExists(
                id,
                paired_id,
            )) => {
                assert_eq!(id, new_device);
                assert_eq!(paired_id, existing_stuff);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&existing_stuff));
        assert!(!game.vanjects.contains_key(&new_device));
    }
}
