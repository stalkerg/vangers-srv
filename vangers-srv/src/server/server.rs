use ::tokio::net::TcpListener;
use ::tokio::sync::mpsc;
use ::tokio::time;
use ::tracing::{error, info};
use std::time::{Duration, Instant};

use crate::client::{Client, ClientID, Connection, MpscData};
use crate::game::Game;
use crate::server::callback::*;
use crate::utils::Uptime;
use crate::{ServerConfig, protocol::*};

use super::games::Games;

enum Event {
    Add(Client),
    #[allow(dead_code)]
    Halt,
}

pub struct Server {
    pub(in crate::server) conf: ServerConfig,
    /// List of all games on the server.
    pub(in crate::server) games: Games,
    /// Counter that storages an uniq game_id for next new game.
    /// TODO: Replace to iterator (i++) (?)
    games_id_uniq: u32,
    /// List of all connected TCP clients.
    pub(in crate::server) clients: Vec<Client>,
    /// Uptime server
    uptime: Uptime,
    // get_game_uniq_id: Box<dyn Fn() -> i32>
}

const DISCONNECTED_PLAYER_TTL: Duration = Duration::from_secs(60);

impl Server {
    pub fn new(conf: ServerConfig) -> Self {
        Self {
            conf,
            games: Games::new(),
            games_id_uniq: 0,
            clients: vec![],
            uptime: Uptime::new(),
            // shell: None,
            // get_game_uniq_id: Box::new(q),
        }
    }

    // pub fn enable_shell(&mut self) {
    //     self.shell = Some(::tokio::spawn(async move {
    //         println!("Interactive shell enabled");
    //         let stdin = io::stdin();
    //         let stdout = io::stdout();

    //         loop {
    //             {
    //                 let mut stdout = stdout.lock();
    //                 stdout.write_all("vangers-srv shell> ".as_bytes()).unwrap();
    //                 stdout.flush().unwrap();
    //             }

    //             let mut input = String::new();
    //             match stdin.read_line(&mut input) {
    //                 Ok(_) => match input.trim() {
    //                     "" => continue,
    //                     "quit" | "exit" => {
    //                         println!("Interactive shell will be terminated");
    //                         return;
    //                     }
    //                     cmd => {
    //                         let cmd = std::iter::once("").chain(cmd.split_whitespace());

    //                         match ShellCmd::try_parse_from(cmd) {
    //                             Ok(shell) => println!("OK command: {:?}", shell),
    //                             Err(err) => println!("Error command: {}", err),
    //                         }
    //                     }
    //                 },
    //                 Err(error) => println!("error: {}", error),
    //             }
    //         }
    //     }));
    // }

    /// Returns the time since server was started in milliseconds.
    pub fn uptime(&self) -> u32 {
        self.uptime.as_secs_u32()

        // // sef.start_time is Instant;
        // let uptime = self.start_time.elapsed().as_millis();
        // u32::try_from(uptime).unwrap_or_else(|_| {
        //     // reset uptime to zero if u32 overflow has been
        //     // detected (~49 days)
        //     // the idia is the same as `SDL_GetTicks` method
        //     self.start_time = Instant::now();
        //     0
        // })
    }

    pub(in crate::server) fn get_game_by_clientid(&self, client_id: ClientID) -> Option<&Game> {
        self.games.get_game_by_client_id(client_id)
    }

    pub(in crate::server) fn get_mut_game_by_clientid(
        &mut self,
        client_id: ClientID,
    ) -> Option<&mut Game> {
        self.games.get_mut_game_by_client_id(client_id)
    }

    pub(in crate::server) fn get_game_uniq_id(&mut self) -> u32 {
        self.games_id_uniq += 1;
        self.games_id_uniq
    }

    fn mark_player_disconnected(&mut self, client_id: ClientID) {
        if let Some(player) = self.games.get_mut_player_by_client_id(client_id) {
            player.disconnected_until = Some(Instant::now() + DISCONNECTED_PLAYER_TTL);
        }
    }

    fn expire_disconnected_players(&mut self) {
        let now = Instant::now();
        let expired_client_ids = self
            .games
            .iter()
            .flat_map(|(_, game)| game.players.iter())
            .filter_map(|player| match player.disconnected_until {
                Some(deadline) if deadline <= now => Some(player.client_id),
                _ => None,
            })
            .collect::<Vec<_>>();

        for client_id in expired_client_ids {
            if let Err(err) = self.close_socket(&Packet::new(Action::CLOSE_SOCKET, &[]), client_id) {
                error!("failed to expire disconnected player client_id=`{}`: {}", client_id, err);
            }
        }
    }

    pub fn notify(
        &self,
        client_id: ClientID,
        packet: &Packet,
        filter: Box<dyn Fn(&ClientID) -> bool>,
    ) {
        let game = match self.get_game_by_clientid(client_id) {
            Some(game) => game,
            None => {
                error!(
                    "cannot doing notify: player with client_id=`{}` not found on the server",
                    client_id
                );
                return;
            }
        };

        let client_ids = game
            .players
            .iter()
            .map(|p| p.client_id)
            .filter(filter)
            .collect::<Vec<_>>();

        self.clients
            .iter()
            .filter(|c| client_ids.contains(&c.id))
            .for_each(|c| c.send(packet));
    }

    /// Sends `packet` to the current client only.
    pub fn notify_player(&self, client_id: ClientID, packet: &Packet) {
        self.notify(client_id, packet, Box::new(move |&id| id == client_id));
    }

    /// Sends `packet` to all clients in the game exclude a caller client `client_id`.
    pub fn notify_game(&self, client_id: ClientID, packet: &Packet) {
        self.notify(client_id, packet, Box::new(move |&id| id != client_id));
    }

    /// Sends `packet` to all clients.
    pub fn notify_all(&self, client_id: ClientID, packet: &Packet) {
        self.notify(client_id, packet, Box::new(|_| true));
    }

    /// Sends `packet` to players that are currently attached to the same world.
    pub fn notify_world(
        &self,
        client_id: ClientID,
        world_id: u8,
        packet: &Packet,
        include_sender: bool,
    ) {
        let game = match self.get_game_by_clientid(client_id) {
            Some(game) => game,
            None => {
                error!(
                    "cannot doing notify_world: player with client_id=`{}` not found on the server",
                    client_id
                );
                return;
            }
        };

        let client_ids = client_ids_in_world(game, world_id, Some(client_id), include_sender);
        self.clients
            .iter()
            .filter(|c| client_ids.contains(&c.id))
            .for_each(|c| c.send(packet));
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (client_tx, mut clients_rx) = mpsc::channel(50);
        let (event_tx, mut event_rx) = mpsc::channel::<Event>(10);

        let endpoint = format!("0.0.0.0:{}", self.conf.port);
        println!("Server is listening on: {}", endpoint);
        let listener = TcpListener::bind(endpoint).await?;
        let mut disconnected_cleanup_tick = time::interval(Duration::from_secs(1));

        ::tokio::spawn(async move {
            // listening for connecting new clients
            loop {
                if let Ok((stream, addr)) = listener.accept().await {
                    info!(peer=%addr, "====== new client connected ======");
                    let client = Client::new(stream, client_tx.clone());
                    if event_tx.send(Event::Add(client)).await.is_err() {
                        error!("Terminate tcp-listener because of `event_rx` was closed.");
                        break;
                    }
                }
            }
        });

        loop {
            ::tokio::select! {
                _ = disconnected_cleanup_tick.tick() => {
                    self.expire_disconnected_players();
                }
                event = event_rx.recv() => {
                    match event {
                        Some(Event::Add(client)) => {
                            self.clients.push(client);
                        }
                        Some(Event::Halt) => {
                            return Ok(());
                        }
                        // Ok(Event::ShellCmd(cmd)) => self.do_shell(cmd),
                        None => {
                            error!("unexpected event_rx channel closed");
                            return Ok(());
                        }
                    }
                }
                data = clients_rx.recv() => {
                    match data {
                        Some(MpscData(id, Connection::Disconnected)) => {
                            self.mark_player_disconnected(id);
                            self.clients.retain(|c| c.id != id);
                        }
                        Some(MpscData(id, connection @ Connection::Authenticated(_))
                        | MpscData(id, connection @ Connection::Connected)) => {
                            let client = self.clients.iter_mut().find(|c| c.id == id);
                            if let Some(client) = client {
                                client.connection = connection;
                                if let Connection::Authenticated(protocol) = client.connection {
                                    client.protocol = protocol;
                                }
                            }
                        }
                        Some(MpscData(id, Connection::Updated(p))) => {
                            self.on_update(id, p);
                        }
                        None => {
                            error!("unexpected clients_rx channel closed");
                            return Ok(());
                        }
                    }
                }

            }
        }
    }
}

fn client_ids_in_world(
    game: &Game,
    world_id: u8,
    sender_id: Option<ClientID>,
    include_sender: bool,
) -> Vec<ClientID> {
    game.players
        .iter()
        .filter(|player| {
            player
                .world
                .as_ref()
                .map(|world| world.borrow().id == world_id)
                .unwrap_or(false)
        })
        .filter(|player| include_sender || Some(player.client_id) != sender_id)
        .map(|player| player.client_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{client_ids_in_world, Server};
    use crate::game::{Game, World};
    use crate::player::Player;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    #[test]
    fn selects_only_players_from_requested_world() {
        let mut game = Game::new(1);
        game.attach_player(Player::new(11));
        game.attach_player(Player::new(22));

        let world1 = Rc::new(RefCell::new(World::new(1, 100)));
        let world2 = Rc::new(RefCell::new(World::new(2, 100)));
        game.worlds.push(Rc::clone(&world1));
        game.worlds.push(Rc::clone(&world2));
        game.place_player(11, &world1.borrow());
        game.place_player(22, &world2.borrow());

        let ids = client_ids_in_world(&game, 1, Some(11), false);
        assert!(ids.is_empty());

        let ids = client_ids_in_world(&game, 1, Some(11), true);
        assert_eq!(ids, vec![11]);
    }

    #[test]
    fn expire_disconnected_players_forces_close_after_ttl() {
        let mut srv = Server::new(Default::default());
        let mut game = Game::new(1);
        let client_id = 11;
        game.attach_player(Player::new(client_id));
        {
            let player = game.get_mut_player(client_id).unwrap();
            player.disconnected_until = Some(Instant::now() - Duration::from_secs(1));
        }
        srv.games.insert(1, game);

        srv.expire_disconnected_players();

        assert!(srv.games.get(&1).is_none());
    }
}
