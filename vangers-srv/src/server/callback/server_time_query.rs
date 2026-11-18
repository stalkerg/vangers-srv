use crate::Server;
use crate::client::ClientID;
use crate::protocol::Packet;

use super::{OnUpdateError, OnUpdateOk};

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_ServerTimeQuery {
    fn server_time_query(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_ServerTimeQuery for Server {
    #[tracing::instrument(skip_all)]
    fn server_time_query(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let mut data = (self.uptime() * 256).to_le_bytes().to_vec();
        let capability = crate::network_text::CAPABILITY;
        if packet.data == capability && self.get_game_by_clientid(client_id).is_none() {
            if let Some(client) = self
                .clients
                .iter_mut()
                .find(|client| client.id == client_id)
            {
                client.unicode_text = true;
                data.extend_from_slice(capability);
            }
        }

        packet
            .create_answer(data)
            .map(OnUpdateOk::Response)
            .ok_or(OnUpdateError::ResponsePacketTypeNotExist(packet.action))
    }
}
