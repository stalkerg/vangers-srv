use crate::client::ClientID;
use crate::protocol::Packet;
use crate::{Server, player::Player};
use crate::{network_text, utils};

use super::{OnUpdateError, OnUpdateOk};

#[derive(Debug, ::thiserror::Error)]
pub enum DirectSendingError {
    #[error("given data is too small")]
    Parse,
    #[error("cannot parse c-string")]
    String,
    #[error("invalid UTF-8 direct message")]
    InvalidUnicode,
    #[error("client with client_id=`{0}` is out of all games")]
    ClientIsOutOfGames(ClientID),
    #[error("player with client_id=`{0}` is not found in game_id=`{1}`")]
    PlayerNotFound(ClientID, u32),
    #[error("player with client_id=`{0}` is not bind in game_id=`{1}`")]
    PlayerNotBind(ClientID, u32),
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_DirectSending {
    fn direct_sending(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_DirectSending for Server {
    #[tracing::instrument(skip_all)]
    fn direct_sending(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        if packet.data.len() < 4 + 1 + 1 {
            return Err(DirectSendingError::Parse.into());
        }

        let game = self
            .get_game_by_clientid(client_id)
            .ok_or(DirectSendingError::ClientIsOutOfGames(client_id))?;

        let mask = utils::slice_le_to_u32(&packet.data[0..4]);

        // storages binded player_id by client_id
        let mut player: Option<&Player> = None;
        // storages all client_ids for sending to
        let mut client_ids = vec![];

        for p in &game.players {
            if let Some(bind) = p.bind {
                if p.client_id == client_id {
                    // we find transmitter client
                    player = Some(p);
                } else if bind.mask() as u32 & mask != 0 {
                    // we find valid reciever client
                    client_ids.push(p.client_id)
                }
            }
        }

        let player = player.ok_or(DirectSendingError::PlayerNotFound(client_id, game.id))?;
        if player.bind.is_none() || player.auth.is_none() {
            Err(DirectSendingError::PlayerNotBind(client_id, game.id))?
        }
        let player_id = player
            .bind
            .expect("we check for none a few rows above")
            .id();

        let Some(msg) = utils::get_first_cstr(&packet.data[4..]) else {
            Err(DirectSendingError::String)?
        };

        let unicode = self.unicode_text(client_id);
        let (legacy, utf8) =
            network_text::chat_for_modes(msg, unicode).ok_or(DirectSendingError::InvalidUnicode)?;
        let answer = |msg: &[u8]| {
            let mut bytes = vec![player_id];
            bytes.extend_from_slice(msg);
            packet
                .create_answer(bytes)
                .expect("DIRECT_SENDING has a response")
        };
        let legacy = answer(&legacy);
        let utf8 = answer(&utf8);

        self.clients
            .iter()
            .filter(|c| client_ids.contains(&c.id))
            .for_each(|c| c.send(if c.unicode_text { &utf8 } else { &legacy }));

        Ok(OnUpdateOk::Complete)
    }
}
