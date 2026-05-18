use crate::Server;
use crate::client::ClientID;
use crate::game::Type as GameType;
use crate::player::Body as PlayerBody;
use crate::protocol::{NetTransportReceive, Packet};

use super::{OnUpdateError, OnUpdateOk};

const TOP_LIST_LIMIT: usize = 10;

#[derive(Debug, ::thiserror::Error)]
pub enum TopListQueryError {
    #[error("required byte `mp_game` not found")]
    ModeEmpty,
}

#[derive(Debug, Clone)]
struct TopEntry {
    name: Vec<u8>,
    rating: f32,
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_TopListQuery {
    fn top_list_query(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_TopListQuery for Server {
    #[tracing::instrument(skip_all)]
    fn top_list_query(
        &mut self,
        packet: &Packet,
        _client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let mp_game = *packet.data.first().ok_or(TopListQueryError::ModeEmpty)?;

        let mut entries = self
            .games
            .values()
            .filter(|game| game.is_configured() && game.get_gmtype() as u8 == mp_game)
            .flat_map(|game| {
                let current = game.players.iter().filter_map(|player| {
                    let auth = player.auth.as_ref()?;
                    let body = player.body.as_ref()?;
                    Some(TopEntry {
                        name: auth.name().to_vec(),
                        rating: body.rating(),
                    })
                });

                let removed = game.removed_players.iter().filter_map(|player| {
                    let body = PlayerBody::from_slice(&player.body)?;
                    Some(TopEntry {
                        name: player.name.clone(),
                        rating: body.rating(),
                    })
                });

                current.chain(removed).collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        entries.sort_by(|a, b| {
            b.rating
                .partial_cmp(&a.rating)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries.truncate(TOP_LIST_LIMIT);

        let mut data = vec![mp_game, entries.len() as u8];
        for entry in entries {
            data.extend_from_slice(&entry.name);
            data.extend_from_slice(&entry.rating.to_le_bytes());
        }

        packet
            .create_answer(data)
            .map(OnUpdateOk::Response)
            .ok_or(OnUpdateError::ResponsePacketTypeNotExist(packet.action))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Config, Game};
    use crate::player::{Body as PlayerBody, Player, Status};
    use crate::protocol::Action;

    fn body_with_rating(rating: f32) -> PlayerBody {
        let mut bytes = vec![1u8, 2u8, 3u8, 4u8]
            .iter()
            .chain(&5u32.to_le_bytes())
            .chain(&rating.to_le_bytes())
            .chain(&[7u8])
            .chain(&8i16.to_le_bytes())
            .chain(&9i16.to_le_bytes())
            .chain(&10u32.to_le_bytes())
            .chain(&11u32.to_le_bytes())
            .copied()
            .collect::<Vec<_>>();
        bytes.extend_from_slice(&[0; 16]);
        PlayerBody::from_slice(&bytes).unwrap()
    }

    fn request(mode: GameType) -> Packet {
        Packet::new(Action::TOP_LIST_QUERY, &[mode as u8])
    }

    #[test]
    fn top_list_response_contains_sorted_current_and_removed_players() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        game.config = Some(Config::new(GameType::PASSEMBLOSS));

        let mut p1 = Player::new(11);
        p1.set_auth(b"alpha\0", b"\0");
        p1.body = Some(body_with_rating(5.0));
        game.attach_player(p1);

        let mut p2 = Player::new(22);
        p2.set_auth(b"beta\0", b"\0");
        p2.body = Some(body_with_rating(9.0));
        game.attach_player(p2);

        let mut p3 = Player::new(33);
        p3.set_auth(b"gamma\0", b"\0");
        p3.body = Some(body_with_rating(7.0));
        p3.status = Status::FINISHED;
        game.attach_player(p3);
        let snapshot =
            crate::game::RemovedPlayer::from_player(game.get_player(33).unwrap()).unwrap();
        game.removed_players.push(snapshot);
        game.players.retain(|p| p.client_id != 33);

        srv.games.insert(1, game);

        let response = srv
            .top_list_query(&request(GameType::PASSEMBLOSS), 1)
            .unwrap();
        let packet = match response {
            OnUpdateOk::Response(packet) => packet,
            other => panic!("unexpected response: {other:?}"),
        };

        assert_eq!(packet.action, Action::TOP_LIST_RESPONSE);
        assert_eq!(packet.data[0], GameType::PASSEMBLOSS as u8);
        assert_eq!(packet.data[1], 3);

        let mut cursor = 2usize;
        let mut names = Vec::new();
        let mut ratings = Vec::new();
        for _ in 0..3 {
            let end = packet.data[cursor..]
                .iter()
                .position(|&b| b == 0)
                .map(|pos| cursor + pos + 1)
                .unwrap();
            names.push(packet.data[cursor..end].to_vec());
            cursor = end;
            let mut rating = [0u8; 4];
            rating.copy_from_slice(&packet.data[cursor..cursor + 4]);
            ratings.push(f32::from_le_bytes(rating));
            cursor += 4;
        }

        assert_eq!(
            names,
            vec![b"beta\0".to_vec(), b"gamma\0".to_vec(), b"alpha\0".to_vec()]
        );
        assert_eq!(ratings, vec![9.0, 7.0, 5.0]);
    }

    #[test]
    fn top_list_response_filters_by_game_mode() {
        let mut srv = Server::new(Default::default());

        let mut game1 = Game::new(1);
        game1.config = Some(Config::new(GameType::PASSEMBLOSS));
        let mut p1 = Player::new(11);
        p1.set_auth(b"pass\0", b"\0");
        p1.body = Some(body_with_rating(9.0));
        game1.attach_player(p1);
        srv.games.insert(1, game1);

        let mut game2 = Game::new(2);
        game2.config = Some(Config::new(GameType::MECHOSOMA));
        let mut p2 = Player::new(22);
        p2.set_auth(b"mech\0", b"\0");
        p2.body = Some(body_with_rating(99.0));
        game2.attach_player(p2);
        srv.games.insert(2, game2);

        let response = srv
            .top_list_query(&request(GameType::PASSEMBLOSS), 1)
            .unwrap();
        let packet = match response {
            OnUpdateOk::Response(packet) => packet,
            other => panic!("unexpected response: {other:?}"),
        };

        assert_eq!(packet.action, Action::TOP_LIST_RESPONSE);
        assert_eq!(packet.data[0], GameType::PASSEMBLOSS as u8);
        assert_eq!(packet.data[1], 1);
    }
}
