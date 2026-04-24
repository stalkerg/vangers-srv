use crate::Server;
use crate::client::ClientID;
use crate::protocol::{Action, Packet};
use crate::utils::slice_le_to_i32;

use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum RestoreConnectionError {
    #[error("required payload `game_id: i32, player_id: u8` not found")]
    PayloadInvalid,
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_RestoreConnection {
    fn restore_connection(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_RestoreConnection for Server {
    #[tracing::instrument(skip_all)]
    fn restore_connection(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        if packet.data.len() != 5 {
            return Err(RestoreConnectionError::PayloadInvalid.into());
        }

        let game_id = slice_le_to_i32(&packet.data[..4]) as u32;
        let player_id = packet.data[4];

        let restored_client_id = self
            .games
            .get_mut_game_by_id(game_id)
            .and_then(|game| {
                game.players.iter_mut().find(|player| {
                    player
                        .bind
                        .map(|bind| bind.id())
                        .map(|bind_id| bind_id == player_id)
                        .unwrap_or(false)
                })
            })
            .map(|player| {
                let old_client_id = player.client_id;
                player.client_id = client_id;
                player.disconnected_until = None;
                old_client_id
            });

        if let Some(old_client_id) = restored_client_id {
            if old_client_id != client_id {
                self.clients.retain(|client| client.id != old_client_id);
            }
        }

        let restored = restored_client_id.is_some();

        Ok(OnUpdateOk::Response(Packet::new(
            Action::RESTORE_CONNECTION_RESPONSE,
            &[if restored { 1 } else { 0 }],
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Game;
    use crate::player::Player;
    use std::time::{Duration, Instant};

    #[test]
    fn restore_connection_rebinds_player_to_new_client_id() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let old_client_id: ClientID = 11;
        let new_client_id: ClientID = 22;
        let bind_id = game.attach_player(Player::new(old_client_id)).unwrap();
        game.get_mut_player(old_client_id).unwrap().disconnected_until =
            Some(Instant::now() + Duration::from_secs(60));
        srv.games.insert(1, game);

        let packet = Packet::new(Action::RESTORE_CONNECTION, &[
            1u32.to_le_bytes()[0],
            1u32.to_le_bytes()[1],
            1u32.to_le_bytes()[2],
            1u32.to_le_bytes()[3],
            bind_id,
        ]);

        let response = srv.restore_connection(&packet, new_client_id).unwrap();

        match response {
            OnUpdateOk::Response(packet) => {
                assert_eq!(packet.action, Action::RESTORE_CONNECTION_RESPONSE);
                assert_eq!(packet.data, vec![1]);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let game = srv.games.get(&1).unwrap();
        let player = game.get_player(new_client_id).unwrap();
        assert_eq!(player.bind.unwrap().id(), bind_id);
        assert!(player.disconnected_until.is_none());
        assert!(game.get_player(old_client_id).is_none());
    }

    #[test]
    fn restore_connection_returns_zero_for_missing_player() {
        let mut srv = Server::new(Default::default());
        srv.games.insert(1, Game::new(1));

        let packet = Packet::new(Action::RESTORE_CONNECTION, &[
            1u32.to_le_bytes()[0],
            1u32.to_le_bytes()[1],
            1u32.to_le_bytes()[2],
            1u32.to_le_bytes()[3],
            1,
        ]);

        let response = srv.restore_connection(&packet, 22).unwrap();

        match response {
            OnUpdateOk::Response(packet) => {
                assert_eq!(packet.action, Action::RESTORE_CONNECTION_RESPONSE);
                assert_eq!(packet.data, vec![0]);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
