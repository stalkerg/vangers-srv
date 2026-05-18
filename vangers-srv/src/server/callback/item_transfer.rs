use ::tracing::info;

use crate::Server;
use crate::client::ClientID;
use crate::protocol::{Action, Packet};
use crate::utils::slice_le_to_i32;
use crate::vanject::{DecodedVanjectId, NID, Vanject, VanjectError, get_vanject_type};

use super::{OnUpdateError, OnUpdateOk};

const ITEM_TRANSFER_PICKUP: u8 = 1;
const ITEM_TRANSFER_DROP: u8 = 2;
const ITEM_TRANSFER_HEADER_SIZE: usize = 10;
pub(super) const ITEM_STATE_IN_INVENTORY: u8 = 1;
pub(super) const ITEM_STATE_IN_WORLD: u8 = 2;
pub(super) const ITEM_REMOVED_REASON_DELETE: u8 = 1;

#[derive(Debug, ::thiserror::Error)]
pub enum ItemTransferError {
    #[error("ITEM_TRANSFER packet is too small: {0} bytes")]
    PacketTooSmall(usize),
    #[error("unsupported ITEM_TRANSFER kind: {0}")]
    InvalidKind(u8),
    #[error("fail read new item vanject: [{0}]")]
    SliceToVanjectParse(VanjectError),
    #[error("player with `client_id`={0} not found")]
    PlayerNotFound(ClientID),
    #[error("player with `client_id`={0} not bind")]
    PlayerNotBind(ClientID),
    #[error("player with `client_id`={0} is out of all worlds")]
    PlayerWorldEmpty(ClientID),
    #[error("ITEM_TRANSFER type mismatch: kind={0}, old_type={1}, new_type={2}")]
    TypeMismatch(u8, i32, i32),
    #[error("ITEM_TRANSFER linked id mismatch: old_id={0}, linked_id={1}")]
    LinkedIdMismatch(i32, i32),
    #[error("ITEM_TRANSFER create body is too small")]
    NewItemBodyTooSmall,
    #[error("ITEM_TRANSFER delete marker must be 1, got {0}")]
    InvalidDeleteMarker(u8),
    #[error("ITEM_TRANSFER id pair mismatch: old_id={0}, new_id={1}")]
    ItemIdPairMismatch(i32, i32),
    #[error("old item vanject with `id`={0} not found")]
    OldItemNotFound(i32),
    #[error("new item vanject with `id`={0} already exists")]
    NewItemAlreadyExists(i32),
    #[error("vanject with `id`={0} is not owned by player_bind_id={1}")]
    NotOwner(i32, u8),
    #[error("vanject with `id`={0} belongs to world `{1}`, but player is in world `{2}`")]
    WrongWorld(i32, u8, u8),
}

struct ItemTransfer {
    kind: u8,
    old_id: i32,
    delete_body: u8,
    new_vanject: Vanject,
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_ItemTransfer {
    fn item_transfer(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_ItemTransfer for Server {
    #[tracing::instrument(skip_all)]
    fn item_transfer(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let transfer = parse_item_transfer(packet)?;
        validate_transfer_types(&transfer)?;
        validate_transfer_id_pair(&transfer)?;
        validate_linked_item_id(&transfer)?;
        validate_delete_marker(&transfer)?;

        let old_decoded = DecodedVanjectId::new(transfer.old_id);
        let new_decoded = DecodedVanjectId::new(transfer.new_vanject.id);

        let (transfer_world, apply_packet) = {
            let game = self
                .get_mut_game_by_clientid(client_id)
                .ok_or(ItemTransferError::PlayerNotFound(client_id))?;

            let (player_bind_id, player_world_id) = {
                let player = game
                    .get_player(client_id)
                    .ok_or(ItemTransferError::PlayerNotFound(client_id))?;
                (
                    player
                        .bind
                        .map(|bind| bind.id())
                        .ok_or(ItemTransferError::PlayerNotBind(client_id))?,
                    player
                        .world
                        .as_ref()
                        .map(|world| world.borrow().id)
                        .ok_or(ItemTransferError::PlayerWorldEmpty(client_id))?,
                )
            };

            if game.vanjects.contains_key(&transfer.new_vanject.id) {
                log_transfer_decision(
                    "ITEM_TRANSFER",
                    transfer.kind,
                    &old_decoded,
                    &new_decoded,
                    -1,
                    player_bind_id,
                    client_id,
                    player_world_id,
                    "rejected_new_item_already_exists",
                );
                Err(ItemTransferError::NewItemAlreadyExists(
                    transfer.new_vanject.id,
                ))?;
            }

            let old_vanject = match game.vanjects.get(&transfer.old_id) {
                Some(vanject) => vanject,
                None => {
                    log_transfer_decision(
                        "ITEM_TRANSFER",
                        transfer.kind,
                        &old_decoded,
                        &new_decoded,
                        -1,
                        player_bind_id,
                        client_id,
                        player_world_id,
                        "rejected_old_item_missing",
                    );
                    Err(ItemTransferError::OldItemNotFound(transfer.old_id))?
                }
            };

            let old_world = old_vanject.get_world() as u8;
            let new_world = transfer.new_vanject.get_world() as u8;

            if old_world != player_world_id {
                log_transfer_decision(
                    "ITEM_TRANSFER",
                    transfer.kind,
                    &old_decoded,
                    &new_decoded,
                    old_vanject.player_bind_id as i32,
                    player_bind_id,
                    client_id,
                    player_world_id,
                    "rejected_old_wrong_world",
                );
                Err(ItemTransferError::WrongWorld(
                    transfer.old_id,
                    old_world,
                    player_world_id,
                ))?;
            }

            if new_world != player_world_id {
                log_transfer_decision(
                    "ITEM_TRANSFER",
                    transfer.kind,
                    &old_decoded,
                    &new_decoded,
                    old_vanject.player_bind_id as i32,
                    player_bind_id,
                    client_id,
                    player_world_id,
                    "rejected_new_wrong_world",
                );
                Err(ItemTransferError::WrongWorld(
                    transfer.new_vanject.id,
                    new_world,
                    player_world_id,
                ))?;
            }

            if transfer.kind == ITEM_TRANSFER_DROP && old_vanject.player_bind_id != player_bind_id {
                log_transfer_decision(
                    "ITEM_TRANSFER",
                    transfer.kind,
                    &old_decoded,
                    &new_decoded,
                    old_vanject.player_bind_id as i32,
                    player_bind_id,
                    client_id,
                    player_world_id,
                    "rejected_non_owner_drop",
                );
                Err(ItemTransferError::NotOwner(transfer.old_id, player_bind_id))?;
            }

            let stored_owner = old_vanject.player_bind_id;
            let mut new_vanject = transfer.new_vanject;
            new_vanject.player_bind_id = player_bind_id;

            game.vanjects.remove(&transfer.old_id);

            let apply_packet = item_state_packet(transfer.old_id, &new_vanject);

            game.vanjects.insert(new_vanject.id, new_vanject);

            log_transfer_decision(
                "ITEM_TRANSFER",
                transfer.kind,
                &old_decoded,
                &new_decoded,
                stored_owner as i32,
                player_bind_id,
                client_id,
                player_world_id,
                "accepted_atomic_transfer",
            );

            (new_world, apply_packet)
        };

        self.notify_world(client_id, transfer_world, &apply_packet, false);

        Ok(OnUpdateOk::Complete)
    }
}

pub(super) fn item_state_packet(previous_id: i32, vanject: &Vanject) -> Packet {
    let state = item_state_for_vanject(vanject);
    let data = std::iter::empty()
        .chain(&[state])
        .chain(&previous_id.to_le_bytes())
        .chain(&vanject.to_vangers_byte())
        .copied()
        .collect::<Vec<_>>();
    Packet::new(Action::ITEM_STATE, &data)
}

pub(super) fn item_removed_packet(vanject: &Vanject, reason: u8) -> Packet {
    let paired_id = item_linked_id(vanject).unwrap_or(0);
    let data = std::iter::empty()
        .chain(&vanject.id.to_le_bytes())
        .chain(&paired_id.to_le_bytes())
        .chain(&[reason])
        .copied()
        .collect::<Vec<_>>();
    Packet::new(Action::ITEM_REMOVED, &data)
}

pub(super) fn is_item_type(vanject_type: i32) -> bool {
    matches!(vanject_type, NID::STUFF | NID::DEVICE)
}

pub(super) fn is_item_vanject(vanject: &Vanject) -> bool {
    is_item_type(vanject.get_type())
}

pub(super) fn item_linked_id(vanject: &Vanject) -> Option<i32> {
    if vanject.body.len() < 5 {
        return None;
    }
    Some(slice_le_to_i32(&vanject.body[1..5]))
}

pub(super) fn item_has_paired_vanject(
    vanject: &Vanject,
    vanjects: &std::collections::HashMap<i32, Vanject>,
) -> Option<i32> {
    let linked_id = item_linked_id(vanject)?;
    if is_item_type(get_vanject_type(linked_id)) && vanjects.contains_key(&linked_id) {
        Some(linked_id)
    } else {
        None
    }
}

fn item_state_for_vanject(vanject: &Vanject) -> u8 {
    match vanject.get_type() {
        NID::DEVICE => ITEM_STATE_IN_INVENTORY,
        NID::STUFF => ITEM_STATE_IN_WORLD,
        _ => 0,
    }
}

fn parse_item_transfer(packet: &Packet) -> Result<ItemTransfer, ItemTransferError> {
    if packet.data.len() < ITEM_TRANSFER_HEADER_SIZE + 14 {
        return Err(ItemTransferError::PacketTooSmall(packet.data.len()));
    }

    let kind = packet.data[0];
    if !matches!(kind, ITEM_TRANSFER_PICKUP | ITEM_TRANSFER_DROP) {
        return Err(ItemTransferError::InvalidKind(kind));
    }

    let old_id = slice_le_to_i32(&packet.data[1..5]);
    let delete_body = packet.data[9];
    let new_vanject = Vanject::create_from_slice(&packet.data[ITEM_TRANSFER_HEADER_SIZE..])
        .map_err(ItemTransferError::SliceToVanjectParse)?;

    Ok(ItemTransfer {
        kind,
        old_id,
        delete_body,
        new_vanject,
    })
}

fn validate_transfer_types(transfer: &ItemTransfer) -> Result<(), ItemTransferError> {
    let old_type = get_vanject_type(transfer.old_id);
    let new_type = transfer.new_vanject.get_type();

    match transfer.kind {
        ITEM_TRANSFER_PICKUP if old_type == NID::STUFF && new_type == NID::DEVICE => Ok(()),
        ITEM_TRANSFER_DROP if old_type == NID::DEVICE && new_type == NID::STUFF => Ok(()),
        _ => Err(ItemTransferError::TypeMismatch(
            transfer.kind,
            old_type,
            new_type,
        )),
    }
}

fn validate_transfer_id_pair(transfer: &ItemTransfer) -> Result<(), ItemTransferError> {
    let old_decoded = DecodedVanjectId::new(transfer.old_id);
    let new_decoded = DecodedVanjectId::new(transfer.new_vanject.id);

    if old_decoded.station != new_decoded.station
        || old_decoded.world != new_decoded.world
        || old_decoded.counter != new_decoded.counter
    {
        return Err(ItemTransferError::ItemIdPairMismatch(
            transfer.old_id,
            transfer.new_vanject.id,
        ));
    }

    Ok(())
}

fn validate_linked_item_id(transfer: &ItemTransfer) -> Result<(), ItemTransferError> {
    let linked_id = linked_item_id(&transfer.new_vanject)?;
    if linked_id != transfer.old_id {
        return Err(ItemTransferError::LinkedIdMismatch(
            transfer.old_id,
            linked_id,
        ));
    }
    Ok(())
}

fn validate_delete_marker(transfer: &ItemTransfer) -> Result<(), ItemTransferError> {
    if transfer.delete_body != 1 {
        return Err(ItemTransferError::InvalidDeleteMarker(transfer.delete_body));
    }
    Ok(())
}

fn linked_item_id(vanject: &Vanject) -> Result<i32, ItemTransferError> {
    item_linked_id(vanject).ok_or(ItemTransferError::NewItemBodyTooSmall)
}

fn log_transfer_decision(
    action: &'static str,
    transfer_kind: u8,
    old_decoded: &DecodedVanjectId,
    new_decoded: &DecodedVanjectId,
    stored_owner: i32,
    packet_sender_bind_id: u8,
    packet_sender_client_id: ClientID,
    packet_world: u8,
    decision: &'static str,
) {
    info!(
        action,
        transfer_kind,
        old_id = old_decoded.id,
        old_id_hex = %format_args!("0x{:08X}", old_decoded.id as u32),
        old_station = old_decoded.station,
        old_world = old_decoded.world,
        old_type_name = old_decoded.type_name(),
        old_type_id = old_decoded.type_id,
        old_counter = old_decoded.counter,
        new_id = new_decoded.id,
        new_id_hex = %format_args!("0x{:08X}", new_decoded.id as u32),
        new_station = new_decoded.station,
        new_world = new_decoded.world,
        new_type_name = new_decoded.type_name(),
        new_type_id = new_decoded.type_id,
        new_counter = new_decoded.counter,
        stored_owner,
        packet_sender = packet_sender_client_id,
        packet_sender_bind_id,
        packet_world,
        decision,
        "item transfer"
    );
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

    fn make_transfer_packet(kind: u8, old_id: i32, new_id: i32, linked_id: i32) -> Packet {
        let create = make_item_create_payload(
            new_id,
            linked_id,
            if get_vanject_type(new_id) == NID::DEVICE {
                1
            } else {
                0
            },
        );
        let data = std::iter::empty()
            .chain(&[kind])
            .chain(&old_id.to_le_bytes())
            .chain(&77i32.to_le_bytes())
            .chain(&[1u8])
            .chain(&create)
            .copied()
            .collect::<Vec<_>>();
        Packet::new(Action::ITEM_TRANSFER, &data)
    }

    fn make_server_with_players() -> (Server, ClientID, ClientID) {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let world = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world));

        let player1: ClientID = 11;
        let player2: ClientID = 22;
        game.attach_player(Player::new(player1));
        game.attach_player(Player::new(player2));
        game.place_player(player1, &world.borrow());
        game.place_player(player2, &world.borrow());

        srv.games.insert(1, game);
        (srv, player1, player2)
    }

    fn insert_vanject(srv: &mut Server, id: i32, owner: u8, linked_id: i32, net_owner: i32) {
        let mut vanject =
            Vanject::create_from_slice(&make_item_create_payload(id, linked_id, net_owner))
                .unwrap();
        vanject.player_bind_id = owner;
        srv.games.get_mut(&1).unwrap().vanjects.insert(id, vanject);
    }

    #[test]
    fn pickup_allows_non_owner_world_stuff_and_commits_atomically() {
        let (mut srv, player1, player2) = make_server_with_players();
        let old_stuff = make_id(1, 1, NID::STUFF, 79);
        let new_device = make_id(1, 1, NID::DEVICE, 79);
        insert_vanject(&mut srv, old_stuff, 1, new_device, 0);

        let packet = make_transfer_packet(ITEM_TRANSFER_PICKUP, old_stuff, new_device, old_stuff);
        srv.item_transfer(&packet, player2).unwrap();

        let game = srv.games.get(&1).unwrap();
        assert!(!game.vanjects.contains_key(&old_stuff));
        let device = game.vanjects.get(&new_device).unwrap();
        assert_eq!(device.player_bind_id, 2);
        assert_eq!(linked_item_id(device).unwrap(), old_stuff);
        assert_ne!(player1, player2);
    }

    #[test]
    fn pickup_race_has_single_winner() {
        let (mut srv, _player1, player2) = make_server_with_players();
        let old_stuff = make_id(1, 1, NID::STUFF, 80);
        let winner_device = make_id(1, 1, NID::DEVICE, 80);
        insert_vanject(&mut srv, old_stuff, 1, winner_device, 0);

        let first = make_transfer_packet(ITEM_TRANSFER_PICKUP, old_stuff, winner_device, old_stuff);
        srv.item_transfer(&first, player2).unwrap();

        let second =
            make_transfer_packet(ITEM_TRANSFER_PICKUP, old_stuff, winner_device, old_stuff);
        assert!(srv.item_transfer(&second, player2).is_err());

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&winner_device));
        assert!(!game.vanjects.contains_key(&old_stuff));
    }

    #[test]
    fn item_state_packet_is_single_authoritative_item_event_with_update_payload() {
        let old_stuff = make_id(1, 1, NID::STUFF, 86);
        let new_device = make_id(1, 1, NID::DEVICE, 86);
        let mut vanject =
            Vanject::create_from_slice(&make_item_create_payload(new_device, old_stuff, 1))
                .unwrap();
        vanject.player_bind_id = 2;

        let packet = item_state_packet(old_stuff, &vanject);

        assert_eq!(packet.action, Action::ITEM_STATE);
        assert_eq!(packet.data[0], ITEM_STATE_IN_INVENTORY);
        assert_eq!(slice_le_to_i32(&packet.data[1..5]), old_stuff);
        assert_eq!(slice_le_to_i32(&packet.data[5..9]), new_device);
        assert_eq!(packet.data[9], 2);
        assert_eq!(slice_le_to_i32(&packet.data[10..14]), vanject.time);
        assert_eq!(packet.data.len(), 5 + vanject.to_vangers_byte().len());
    }

    #[test]
    fn item_removed_packet_carries_paired_id_and_reason() {
        let stuff = make_id(1, 1, NID::STUFF, 87);
        let device = make_id(1, 1, NID::DEVICE, 87);
        let vanject =
            Vanject::create_from_slice(&make_item_create_payload(stuff, device, 0)).unwrap();

        let packet = item_removed_packet(&vanject, ITEM_REMOVED_REASON_DELETE);

        assert_eq!(packet.action, Action::ITEM_REMOVED);
        assert_eq!(slice_le_to_i32(&packet.data[0..4]), stuff);
        assert_eq!(slice_le_to_i32(&packet.data[4..8]), device);
        assert_eq!(packet.data[8], ITEM_REMOVED_REASON_DELETE);
    }

    #[test]
    fn drop_requires_inventory_owner() {
        let (mut srv, player1, player2) = make_server_with_players();
        let old_device = make_id(1, 1, NID::DEVICE, 81);
        let new_stuff = make_id(1, 1, NID::STUFF, 81);
        insert_vanject(&mut srv, old_device, 1, new_stuff, 1);

        let packet = make_transfer_packet(ITEM_TRANSFER_DROP, old_device, new_stuff, old_device);
        assert!(srv.item_transfer(&packet, player2).is_err());

        srv.item_transfer(&packet, player1).unwrap();
        let game = srv.games.get(&1).unwrap();
        assert!(!game.vanjects.contains_key(&old_device));
        assert!(game.vanjects.contains_key(&new_stuff));
    }

    #[test]
    fn rejects_wrong_linked_id_without_mutating_state() {
        let (mut srv, _player1, player2) = make_server_with_players();
        let old_stuff = make_id(1, 1, NID::STUFF, 82);
        let new_device = make_id(1, 1, NID::DEVICE, 82);
        let wrong_link = make_id(1, 1, NID::STUFF, 83);
        insert_vanject(&mut srv, old_stuff, 1, new_device, 0);

        let packet = make_transfer_packet(ITEM_TRANSFER_PICKUP, old_stuff, new_device, wrong_link);
        assert!(srv.item_transfer(&packet, player2).is_err());

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&old_stuff));
        assert!(!game.vanjects.contains_key(&new_device));
    }

    #[test]
    fn rejects_wrong_world_without_mutating_state() {
        let (mut srv, _player1, player2) = make_server_with_players();
        let old_stuff = make_id(1, 2, NID::STUFF, 84);
        let new_device = make_id(1, 2, NID::DEVICE, 84);
        insert_vanject(&mut srv, old_stuff, 1, new_device, 0);

        let packet = make_transfer_packet(ITEM_TRANSFER_PICKUP, old_stuff, new_device, old_stuff);
        assert!(srv.item_transfer(&packet, player2).is_err());

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&old_stuff));
        assert!(!game.vanjects.contains_key(&new_device));
    }

    #[test]
    fn rejects_item_id_pair_mismatch_without_mutating_state() {
        let (mut srv, _player1, player2) = make_server_with_players();
        let old_stuff = make_id(1, 1, NID::STUFF, 85);
        let new_device_with_wrong_counter = make_id(1, 1, NID::DEVICE, 86);
        insert_vanject(&mut srv, old_stuff, 1, new_device_with_wrong_counter, 0);

        let packet = make_transfer_packet(
            ITEM_TRANSFER_PICKUP,
            old_stuff,
            new_device_with_wrong_counter,
            old_stuff,
        );
        assert!(srv.item_transfer(&packet, player2).is_err());

        let game = srv.games.get(&1).unwrap();
        assert!(game.vanjects.contains_key(&old_stuff));
        assert!(!game.vanjects.contains_key(&new_device_with_wrong_counter));
    }
}
