#![allow(clippy::collapsible_else_if)]

use ::tracing::{debug, info, warn};

use crate::Server;
use crate::client::ClientID;
use crate::protocol::{Action, NetTransportSend, Packet};
use crate::vanject::*;

use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum CreateObjectError {
    #[error("fail read slice as vanject: [{0}]")]
    SliceToVanjectParse(VanjectError),
    #[error("player with `client_id`={0} not found")]
    PlayerNotFound(ClientID),
    #[error("player with `client_id`={0} not bind")]
    PlayerNotBind(ClientID),
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
                let data = vanject.to_vangers_byte();
                let answer = Packet::new(Action::UPDATE_OBJECT, &data);

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
