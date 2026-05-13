use ::tracing::{debug, info, warn};

use crate::Server;
use crate::client::ClientID;
use crate::protocol::{Action, Packet};
use crate::utils::slice_le_to_i32;
use crate::vanject::{DecodedVanjectId, get_world, is_non_global_vanject};

use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum DeleteObjectError {
    // #[error("fail read slice as vanject")]
    // SliceToVanjectParse,
    #[error("player with `client_id`={0} not found")]
    PlayerNotFound(ClientID),
    #[error("player with `client_id`={0} not bind")]
    PlayerNotBind(ClientID),
    #[error("vanject with `id`={0} is not owned by player_bind_id={1}")]
    NotOwner(i32, u8),
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_DeleteObject {
    fn delete_object(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_DeleteObject for Server {
    #[tracing::instrument(skip_all)]
    fn delete_object(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let vanject_id = slice_le_to_i32(&packet.data[0..4]);
        let decoded = DecodedVanjectId::new(vanject_id);

        let game = match self.get_mut_game_by_clientid(client_id) {
            Some(game) => game,
            None => return Err(DeleteObjectError::PlayerNotFound(client_id).into()),
        };

        let player_auth_id = {
            match game.get_mut_player(client_id).unwrap().bind {
                Some(bind) => bind.id(),
                None => return Err(DeleteObjectError::PlayerNotBind(client_id).into()),
            }
        };

        let data = std::iter::empty()
            .chain(&vanject_id.to_le_bytes())
            .chain(&[player_auth_id])
            .chain(&packet.data[4..8])
            .chain(&packet.data[8..])
            .copied()
            .collect::<Vec<_>>();

        let answer = Packet::new(Action::DELETE_OBJECT, &data);

        // match game.vanjects.remove(&vanject_id) {
        //     Some(v) => {
        //         if v.is_private() {
        //             println!(
        //                 "DELETE OBJECT: deleted PRIVATE vanject: {:?}",
        //                 v.id.to_le_bytes()
        //             );
        //         }
        //     }
        //     None => println!(
        //         "DELETE OBJECT: VANJECT with id=`{:?}` not found",
        //         &vanject_id.to_le_bytes()
        //     ),
        // }

        match game.vanjects.get(&vanject_id) {
            Some(vanject) if vanject.player_bind_id != player_auth_id => {
                warn!(
                    action = "DELETE_OBJECT",
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
                    stored_owner = vanject.player_bind_id,
                    packet_sender = client_id,
                    packet_world = decoded.world,
                    stored_world = vanject.get_world(),
                    decision = "rejected_non_owner",
                    "object lifecycle"
                );
                Err(DeleteObjectError::NotOwner(vanject_id, player_auth_id))?;
            }
            Some(vanject) => {
                info!(
                    action = "DELETE_OBJECT",
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
                    stored_owner = vanject.player_bind_id,
                    packet_sender = client_id,
                    packet_world = decoded.world,
                    stored_world = vanject.get_world(),
                    decision = "accepted",
                    "object lifecycle"
                );
            }
            None => {
                debug!("VANJECT with id=`{}` not found", vanject_id);
                warn!(
                    action = "DELETE_OBJECT",
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
                    stored_owner = -1,
                    packet_sender = client_id,
                    packet_world = decoded.world,
                    stored_world = decoded.world,
                    decision = "ignored_missing",
                    "object lifecycle"
                );
            }
        }

        if game.vanjects.remove(&vanject_id).is_none() {
            debug!("VANJECT with id=`{}` not found", vanject_id);
        }

        if is_non_global_vanject(vanject_id) {
            self.notify_world(client_id, get_world(vanject_id) as u8, &answer, false);
        } else {
            self.notify_game(client_id, &answer);
        }
        Ok(OnUpdateOk::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Game;
    use crate::player::Player;
    use crate::vanject::Vanject;

    fn make_server_with_owned_vanject() -> (Server, ClientID, ClientID, i32) {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);

        let owner_client: ClientID = 11;
        let other_client: ClientID = 22;
        game.attach_player(Player::new(owner_client));
        game.attach_player(Player::new(other_client));

        let mut vanject = Vanject::create_from_slice(&[
            2, 1, 1, 1, // id
            6, 0, 0, 0, // time
            10, 0, // x
            20, 0, // y
            15, 0, // radius
            1, 2, 3,
        ])
        .unwrap();
        vanject.player_bind_id = 1;
        let vanject_id = vanject.id;
        game.vanjects.insert(vanject_id, vanject);

        srv.games.insert(1, game);
        (srv, owner_client, other_client, vanject_id)
    }

    #[test]
    fn rejects_deletes_from_non_owner() {
        let (mut srv, _owner_client, other_client, vanject_id) = make_server_with_owned_vanject();
        let packet = Packet::new(
            Action::DELETE_OBJECT,
            &[
                2, 1, 1, 1, // id
                7, 0, 0, 0, // time
            ],
        );

        let err = srv.delete_object(&packet, other_client).unwrap_err();
        match err {
            OnUpdateError::DeleteObjectError(DeleteObjectError::NotOwner(id, bind_id)) => {
                assert_eq!(id, vanject_id);
                assert_eq!(bind_id, 2);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&vanject_id));
    }
}
