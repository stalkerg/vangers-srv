use crate::Server;
use crate::client::ClientID;
use crate::protocol::Packet;
use crate::utils::slice_le_to_i16;

use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum SetPositionError {
    #[error("invalid SET_POSITION payload size: expected `{expected}`, given `{actual}`")]
    InvalidPayloadSize { expected: usize, actual: usize },
    #[error("player with client_id `{0}` not found")]
    PlayerNotFound(ClientID),
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_SetPosition {
    fn set_position(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_SetPosition for Server {
    #[tracing::instrument(skip_all)]
    fn set_position(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        const PAYLOAD_SIZE: usize = 6;

        if packet.data.len() != PAYLOAD_SIZE {
            return Err(SetPositionError::InvalidPayloadSize {
                expected: PAYLOAD_SIZE,
                actual: packet.data.len(),
            }
            .into());
        }

        let x = slice_le_to_i16(&packet.data[0..2]);
        let y = slice_le_to_i16(&packet.data[2..4]);
        let screen_y_half_size = slice_le_to_i16(&packet.data[4..6]);

        let game = self
            .get_mut_game_by_clientid(client_id)
            .ok_or(SetPositionError::PlayerNotFound(client_id))?;

        let player = game
            .get_mut_player(client_id)
            .ok_or(SetPositionError::PlayerNotFound(client_id))?;

        if player.world.is_none() {
            return Ok(OnUpdateOk::Complete);
        }

        player.pos.x = x;
        player.pos.y = y;
        player.screen_y_half_size = screen_y_half_size;

        Ok(OnUpdateOk::Complete)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::game::{Game, World};
    use crate::player::Player;
    use crate::protocol::{Action, Packet};

    #[test]
    fn updates_player_position_after_world_is_set() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));
        game.place_player(client_id, &world.borrow());
        srv.games.insert(1, game);

        let packet = Packet::new(
            Action::SET_POSITION,
            &std::iter::empty()
                .chain(&123i16.to_le_bytes())
                .chain(&456i16.to_le_bytes())
                .chain(&78i16.to_le_bytes())
                .copied()
                .collect::<Vec<_>>(),
        );

        srv.set_position(&packet, client_id).unwrap();

        let game = srv.games.get(&1).unwrap();
        let player = game.get_player(client_id).unwrap();
        assert_eq!(player.pos.x, 123);
        assert_eq!(player.pos.y, 456);
        assert_eq!(player.screen_y_half_size, 78);
    }

    #[test]
    fn ignores_position_before_world_is_set() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));
        srv.games.insert(1, game);

        let packet = Packet::new(
            Action::SET_POSITION,
            &std::iter::empty()
                .chain(&123i16.to_le_bytes())
                .chain(&456i16.to_le_bytes())
                .chain(&78i16.to_le_bytes())
                .copied()
                .collect::<Vec<_>>(),
        );

        srv.set_position(&packet, client_id).unwrap();

        let game = srv.games.get(&1).unwrap();
        let player = game.get_player(client_id).unwrap();
        assert_eq!(player.pos.x, 0);
        assert_eq!(player.pos.y, 0);
        assert_eq!(player.screen_y_half_size, 0);
    }

    #[test]
    fn rejects_malformed_position_packet() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));
        srv.games.insert(1, game);

        let err = srv
            .set_position(&Packet::new(Action::SET_POSITION, &[1, 2]), client_id)
            .unwrap_err();

        match err {
            OnUpdateError::SetPositionError(SetPositionError::InvalidPayloadSize {
                expected,
                actual,
            }) => {
                assert_eq!(expected, 6);
                assert_eq!(actual, 2);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
