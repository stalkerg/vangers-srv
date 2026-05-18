use std::cell::RefCell;
use std::rc::Rc;

use ::tracing::info;

use crate::Server;
use crate::client::ClientID;
use crate::game::World;
use crate::player::Status as PlayerStatus;
use crate::protocol::{Action, Packet};
use crate::utils::{slice_le_to_i16, slice_le_to_i32};
use crate::vanject::{DecodedVanjectId, NID, Vanject};

use super::item_transfer::{is_item_vanject, item_state_packet};
use super::{OnUpdate_LeaveWorld, OnUpdateError, OnUpdateOk, total_players_data_packet};

#[derive(Debug, ::thiserror::Error)]
pub enum SetWorldError {
    #[error("player with client_id `{0}` not found")]
    PlayerNotFound(ClientID),
    #[error("player with client_id `{0}` not bind")]
    PlayerNotBind(ClientID),
    #[error("invalid world size: expected `{0}`, given `{1}`")]
    InvalidWorldSize(i16, i16),
    // #[error("fail read data slice")]
    // DataParse,
}

#[allow(non_camel_case_types)]
pub(super) trait OnUpdate_SetWorld {
    fn set_world(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError>;
}

impl OnUpdate_SetWorld for Server {
    #[tracing::instrument(skip_all)]
    fn set_world(
        &mut self,
        packet: &Packet,
        client_id: ClientID,
    ) -> Result<OnUpdateOk, OnUpdateError> {
        let world_id = packet.data[0];
        let world_y_size = slice_le_to_i16(&packet.data[1..3]);

        let needs_leave_world = self
            .get_game_by_clientid(client_id)
            .and_then(|game| game.get_player(client_id))
            .and_then(|player| player.world.as_ref())
            .map(|world| world.borrow().id != world_id)
            .unwrap_or(false);

        if needs_leave_world {
            self.leave_world(&Packet::new(Action::LEAVE_WORLD, &[]), client_id)?;
        }

        let (
            player_bind_id,
            world_status,
            status_packet,
            players_world_packet,
            set_world_response,
            snapshot_begin,
            players_snapshot,
            snapshot_vanject,
            snapshot_end,
        ) = {
            let game = self
                .get_mut_game_by_clientid(client_id)
                .ok_or(SetWorldError::PlayerNotFound(client_id))?;

            // Must be sets to `1` if new world was created
            let mut world_status = 0u8;

            // TODO: take out below if-else code into Game struct
            let world = if let Some(world) = game.worlds.iter().find(|w| w.borrow().id == world_id)
            {
                if world.borrow().y_size != world_y_size {
                    Err(SetWorldError::InvalidWorldSize(
                        world.borrow().y_size,
                        world_y_size,
                    ))?
                }
                Rc::clone(world)
            } else {
                // create new world with `world_id`
                let world = Rc::new(RefCell::new(World::new(world_id, world_y_size)));
                game.worlds.push(Rc::clone(&world));
                // game.place_player(client_id, &world.borrow());

                world_status = 1;

                Rc::clone(&world)
            };

            let player = game.get_mut_player(client_id).unwrap();
            let player_bind_id = player
                .bind
                .map(|bind| bind.id())
                .ok_or(SetWorldError::PlayerNotBind(client_id))?;

            // Build a deterministic world snapshot for the joining player.  The
            // legacy replay used to include only world STUFF; this left a window
            // where remote VANGER/SLOT/DEVICE updates could arrive before the
            // local client had objects to attach them to, making other players'
            // weapons or shots disappear.
            let mut snapshot_vanject = game
                .vanjects
                .values()
                .filter(|v| should_include_in_world_snapshot(v, world_id, player_bind_id))
                .collect::<Vec<_>>();
            snapshot_vanject.sort_by_key(|v| (world_snapshot_order(v), v.id));
            let snapshot_vanject = snapshot_vanject
                .into_iter()
                .map(|v| {
                    let packet = if is_item_vanject(v) {
                        item_state_packet(0, v)
                    } else {
                        Packet::new(Action::UPDATE_OBJECT, &v.to_vangers_byte())
                    };
                    (packet, v.player_bind_id)
                })
                .collect::<Vec<_>>();

            let status_packet = if game.place_player(client_id, &world.borrow()) {
                Some(Packet::new(
                    Action::PLAYERS_STATUS,
                    &[player_bind_id, PlayerStatus::GAMING as u8],
                ))
            } else {
                None
            };

            let players_world_packet =
                Packet::new(Action::PLAYERS_WORLD, &[player_bind_id, world_id]);
            let set_world_response = packet.create_answer(vec![world_id, world_status]).unwrap();
            let snapshot_begin = Packet::new(Action::WORLD_SNAPSHOT_BEGIN, &[world_id]);
            let players_snapshot = total_players_data_packet(game);
            let snapshot_end = Packet::new(Action::WORLD_SNAPSHOT_END, &[world_id]);

            (
                player_bind_id,
                world_status,
                status_packet,
                players_world_packet,
                set_world_response,
                snapshot_begin,
                players_snapshot,
                snapshot_vanject,
                snapshot_end,
            )
        };

        if let Some(packet) = status_packet {
            self.notify_all(client_id, &packet);
        }

        self.notify_game(client_id, &players_world_packet);
        self.notify_player(client_id, &set_world_response);
        self.notify_player(client_id, &snapshot_begin);
        self.notify_player(client_id, &players_snapshot);

        snapshot_vanject.iter().for_each(|(p, stored_owner)| {
            let id_offset = if p.action == Action::ITEM_STATE { 5 } else { 0 };
            if p.data.len() >= id_offset + 4 {
                let id = slice_le_to_i32(&p.data[id_offset..id_offset + 4]);
                let decoded = DecodedVanjectId::new(id);
                info!(
                    action = "SET_WORLD replay",
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
                    stored_owner = *stored_owner,
                    packet_sender = client_id,
                    packet_world = world_id,
                    stored_world = decoded.world,
                    decision = "replayed_to_joining_player",
                    "object lifecycle"
                );
            }
            self.notify_player(client_id, p)
        });

        self.notify_player(client_id, &snapshot_end);

        info!(
            action = "SET_WORLD summary",
            packet_sender = client_id,
            player_bind_id,
            requested_world = world_id,
            world_status,
            left_previous_world = needs_leave_world,
            replayed_snapshot_objects = snapshot_vanject.len(),
            notify_game_players_world_packets = 1u8,
            notify_player_set_world_response_packets = 1u8,
            decision = "world_switch_summary",
            "world switch summary"
        );

        Ok(OnUpdateOk::Complete)
    }
}

fn world_snapshot_order(vanject: &Vanject) -> u8 {
    match vanject.get_type() {
        NID::VANGER => 0,
        NID::SLOT => 1,
        NID::DEVICE => 2,
        NID::SHELL => 3,
        NID::STUFF => 4,
        _ => 255,
    }
}

fn should_include_in_world_snapshot(
    vanject: &Vanject,
    world_id: u8,
    joining_player_bind_id: u8,
) -> bool {
    if vanject.get_world() != world_id as i32 {
        return false;
    }

    match vanject.get_type() {
        NID::STUFF => true,
        NID::VANGER | NID::SLOT | NID::DEVICE | NID::SHELL => {
            vanject.player_bind_id != joining_player_bind_id
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Game, World};
    use crate::player::Player;
    use crate::vanject::{NID, Vanject};

    #[test]
    fn set_world_cleans_old_world_when_player_was_already_bound() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id: ClientID = 11;
        game.attach_player(Player::new(client_id));

        let world1 = Rc::new(RefCell::new(World::new(1, 100)));
        game.worlds.push(Rc::clone(&world1));
        game.place_player(client_id, &world1.borrow());

        let mut vanger = Vanject::create_from_slice(&[
            1, 0, 9, 4, // id
            6, 0, 0, 0, // time
            10, 0, // x
            20, 0, // y
            15, 0, // radius
            7, 8,
        ])
        .unwrap();
        vanger.player_bind_id = 1;
        let vanger_id = vanger.id;
        game.vanjects.insert(vanger_id, vanger);

        srv.games.insert(1, game);

        let packet = Packet::new(Action::SET_WORLD, &[2, 100, 0]);
        srv.set_world(&packet, client_id).unwrap();

        let game = srv.games.get(&1).unwrap();
        let player = game.get_player(client_id).unwrap();
        assert_eq!(player.world.as_ref().unwrap().borrow().id, 2);
        assert!(!game.vanjects.contains_key(&vanger_id));
    }

    #[test]
    fn snapshot_includes_remote_vanger_slots_devices_shells_and_world_stuff() {
        let remote_vanger = make_vanject_id(2, 1, NID::VANGER, 1);
        let remote_slot = make_vanject_id(2, 1, NID::SLOT, 2);
        let remote_device = make_vanject_id(2, 1, NID::DEVICE, 3);
        let remote_shell = make_vanject_id(2, 1, NID::SHELL, 4);
        let world_stuff = make_vanject_id(3, 1, NID::STUFF, 5);

        for id in [
            remote_vanger,
            remote_slot,
            remote_device,
            remote_shell,
            world_stuff,
        ] {
            let mut vanject = make_vanject(id);
            vanject.player_bind_id = if id == world_stuff { 3 } else { 2 };
            assert!(
                super::should_include_in_world_snapshot(&vanject, 1, 1),
                "id 0x{id:08X} should be included"
            );
        }
    }

    #[test]
    fn snapshot_orders_vanger_before_equipment_shells_and_world_stuff() {
        let vanger = make_vanject(make_vanject_id(2, 1, NID::VANGER, 1));
        let slot = make_vanject(make_vanject_id(2, 1, NID::SLOT, 2));
        let device = make_vanject(make_vanject_id(2, 1, NID::DEVICE, 3));
        let shell = make_vanject(make_vanject_id(2, 1, NID::SHELL, 4));
        let stuff = make_vanject(make_vanject_id(3, 1, NID::STUFF, 5));

        assert!(super::world_snapshot_order(&vanger) < super::world_snapshot_order(&slot));
        assert!(super::world_snapshot_order(&slot) < super::world_snapshot_order(&device));
        assert!(super::world_snapshot_order(&device) < super::world_snapshot_order(&shell));
        assert!(super::world_snapshot_order(&shell) < super::world_snapshot_order(&stuff));
    }

    #[test]
    fn snapshot_excludes_joining_players_own_inventory_objects_and_other_worlds() {
        let own_vanger = make_vanject_id(1, 1, NID::VANGER, 1);
        let own_device = make_vanject_id(1, 1, NID::DEVICE, 1);
        let other_world_stuff = make_vanject_id(2, 2, NID::STUFF, 2);

        let mut vanject = make_vanject(own_vanger);
        vanject.player_bind_id = 1;
        assert!(!super::should_include_in_world_snapshot(&vanject, 1, 1));

        let mut vanject = make_vanject(own_device);
        vanject.player_bind_id = 1;
        assert!(!super::should_include_in_world_snapshot(&vanject, 1, 1));

        let mut vanject = make_vanject(other_world_stuff);
        vanject.player_bind_id = 2;
        assert!(!super::should_include_in_world_snapshot(&vanject, 1, 1));
    }

    fn make_vanject_id(station: i32, world: i32, nid: i32, counter: i32) -> i32 {
        (station << 26) | (world << 22) | nid | counter
    }

    fn make_vanject(id: i32) -> Vanject {
        Vanject::create_from_slice(&[
            (id & 0xff) as u8,
            ((id >> 8) & 0xff) as u8,
            ((id >> 16) & 0xff) as u8,
            ((id >> 24) & 0xff) as u8,
            6,
            0,
            0,
            0,
            10,
            0,
            20,
            0,
            15,
            0,
            7,
            8,
        ])
        .unwrap()
    }
}
