use crate::protocol::{Action, Packet};
use crate::vanject::VanjectError;
use crate::Server;
use crate::{client::ClientID, utils::slice_le_to_i32};

use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum UpdateObjectError {
    #[error("fail read slice as vanject: [too small slice]")]
    SliceTooSmall,
    #[error("fail read slice as vanject: [{0}]")]
    SliceToVanjectParse(#[from] VanjectError),
    #[error("player with `client_id`={0} not found")]
    PlayerNotFound(ClientID),
    #[error("vanject with `id`={0} not found")]
    VanjectNotFound(i32),
    #[error("player with `client_id`={0} not bind")]
    PlayerNotBind(ClientID),
    #[error("player with `client_id`={0} is out of all worlds")]
    PlayerWorldEmpty(ClientID),
    #[error("vanject with `id`={0} belongs to world `{1}`, but player is in world `{2}`")]
    WrongWorld(i32, u8, u8),
    #[error("vanject with `id`={0} is not owned by player_bind_id={1}")]
    NotOwner(i32, u8),
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_UpdateObject {
    fn update_object(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_UpdateObject for Server {
    #[tracing::instrument(skip_all)]
    fn update_object(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        if packet.data.len() < 4 {
            Err(UpdateObjectError::SliceTooSmall)?
        }

        let vanject_id = slice_le_to_i32(&packet.data[0..4]);

        let game = self
            .get_mut_game_by_clientid(client_id)
            .ok_or(UpdateObjectError::PlayerNotFound(client_id))?;

        let (player_bind_id, player_world_id) = {
            let player = game
                .get_player(client_id)
                .expect("we got game by this player in line above");
            (
                player
                    .bind
                    .map(|bind| bind.id())
                    .ok_or(UpdateObjectError::PlayerNotBind(client_id))?,
                player.world.as_ref().map(|world| world.borrow().id),
            )
        };

        let (packet, is_non_global, world_id) = match game.vanjects.get_mut(&vanject_id) {
            Some(vanject) => {
                let is_non_global = vanject.is_non_global();
                let world_id = vanject.get_world() as u8;

                if is_non_global {
                    let player_world_id =
                        player_world_id.ok_or(UpdateObjectError::PlayerWorldEmpty(client_id))?;
                    if player_world_id != world_id {
                        Err(UpdateObjectError::WrongWorld(
                            vanject_id,
                            world_id,
                            player_world_id,
                        ))?;
                    }
                }

                if is_non_global
                    && vanject.is_non_static()
                    && vanject.player_bind_id != player_bind_id
                {
                    Err(UpdateObjectError::NotOwner(vanject_id, player_bind_id))?;
                }

                vanject
                    .update_from_slice(&packet.data)
                    .map_err(UpdateObjectError::SliceToVanjectParse)?;

                vanject.player_bind_id = player_bind_id;
                (
                    Packet::new(Action::UPDATE_OBJECT, &vanject.to_vangers_byte()),
                    is_non_global,
                    world_id,
                )
            }
            None => Err(UpdateObjectError::VanjectNotFound(vanject_id))?,
        };

        let _ = game;
        if is_non_global {
            self.notify_world(client_id, world_id, &packet, false);
        } else {
            self.notify_game(client_id, &packet);
        }

        Ok(OnUpdateOk::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Game, World};
    use crate::player::Player;
    use crate::vanject::{Vanject, NID};
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

    fn make_update_packet(id: i32, time: i32, x: i16, y: i16, body: &[u8]) -> Packet {
        Packet::new(
            Action::UPDATE_OBJECT,
            &std::iter::empty()
                .chain(&id.to_le_bytes())
                .chain(&time.to_le_bytes())
                .chain(&x.to_le_bytes())
                .chain(&y.to_le_bytes())
                .chain(body)
                .copied()
                .collect::<Vec<_>>(),
        )
    }

    fn make_server_with_owned_vanject() -> (Server, ClientID, ClientID, i32) {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);

        let owner_client: ClientID = 11;
        let other_client: ClientID = 22;
        game.attach_player(Player::new(owner_client));
        game.attach_player(Player::new(other_client));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(owner_client, &world.borrow());
        game.place_player(other_client, &world.borrow());

        let vanject = make_vanject(make_id(1, 1, NID::SLOT, 2), 1);
        let vanject_id = vanject.id;
        game.vanjects.insert(vanject_id, vanject);

        srv.games.insert(1, game);
        (srv, owner_client, other_client, vanject_id)
    }

    #[test]
    fn rejects_updates_from_non_owner() {
        let (mut srv, _owner_client, other_client, vanject_id) = make_server_with_owned_vanject();
        let packet = make_update_packet(vanject_id, 7, 11, 21, &[9, 9]);

        let err = srv.update_object(&packet, other_client).unwrap_err();
        match err {
            OnUpdateError::UpdateObjectError(UpdateObjectError::NotOwner(id, bind_id)) => {
                assert_eq!(id, vanject_id);
                assert_eq!(bind_id, 2);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let game = srv.games.get(&1).unwrap();
        let vanject = game.vanjects.get(&vanject_id).unwrap();
        assert_eq!(vanject.time, 6);
        assert_eq!(vanject.pos.x, 10);
        assert_eq!(vanject.pos.y, 20);
        assert_eq!(vanject.player_bind_id, 1);
    }

    #[test]
    fn allows_static_world_object_update_from_non_owner_in_same_world() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);

        let owner_client: ClientID = 11;
        let other_client: ClientID = 22;
        game.attach_player(Player::new(owner_client));
        game.attach_player(Player::new(other_client));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(owner_client, &world.borrow());
        game.place_player(other_client, &world.borrow());

        let tnt_id = make_id(1, 1, NID::TNT, 2);
        game.vanjects.insert(tnt_id, make_vanject(tnt_id, 1));

        srv.games.insert(1, game);

        let packet = make_update_packet(tnt_id, 7, 11, 21, &[9, 9, 9, 9]);
        srv.update_object(&packet, other_client).unwrap();

        let game = srv.games.get(&1).unwrap();
        let vanject = game.vanjects.get(&tnt_id).unwrap();
        assert_eq!(vanject.time, 7);
        assert_eq!(vanject.pos.x, 11);
        assert_eq!(vanject.pos.y, 21);
        assert_eq!(vanject.body, vec![9, 9, 9, 9]);
        assert_eq!(vanject.player_bind_id, 2);
    }

    #[test]
    fn allows_global_object_update_from_non_owner_without_world() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);

        let owner_client: ClientID = 11;
        let other_client: ClientID = 22;
        game.attach_player(Player::new(owner_client));
        game.attach_player(Player::new(other_client));

        let global_id = make_id(0, 1, NID::GLOBAL, 83);
        game.vanjects.insert(global_id, make_vanject(global_id, 1));

        srv.games.insert(1, game);

        let packet = make_update_packet(global_id, 7, 11, 21, &[9, 9, 9, 9]);
        srv.update_object(&packet, other_client).unwrap();

        let game = srv.games.get(&1).unwrap();
        let vanject = game.vanjects.get(&global_id).unwrap();
        assert_eq!(vanject.time, 7);
        assert_eq!(vanject.pos.x, 11);
        assert_eq!(vanject.pos.y, 21);
        assert_eq!(vanject.body, vec![9, 9, 9, 9]);
        assert_eq!(vanject.player_bind_id, 2);
    }

    #[test]
    fn rejects_non_global_update_from_other_world() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);

        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let player_world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&player_world));
        game.place_player(client_id, &player_world.borrow());

        let sensor_id = make_id(1, 2, NID::SENSOR, 1);
        game.vanjects.insert(sensor_id, make_vanject(sensor_id, 1));

        srv.games.insert(1, game);

        let packet = make_update_packet(sensor_id, 7, 11, 21, &[9]);
        let err = srv.update_object(&packet, client_id).unwrap_err();
        match err {
            OnUpdateError::UpdateObjectError(UpdateObjectError::WrongWorld(
                id,
                world,
                player_world,
            )) => {
                assert_eq!(id, sensor_id);
                assert_eq!(world, 2);
                assert_eq!(player_world, 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
