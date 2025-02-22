use std::f64::consts::TAU;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use log::LevelFilter;
use valence::async_trait;
use valence::block::{BlockPos, BlockState};
use valence::client::{Client, ClientEvent, GameMode, Hand};
use valence::config::{Config, ServerListPing};
use valence::dimension::DimensionId;
use valence::entity::types::Pose;
use valence::entity::{Entity, EntityEvent, EntityId, EntityKind, TrackedData};
use valence::player_list::PlayerListId;
use valence::server::{Server, SharedServer, ShutdownResult};
use valence::text::{Color, TextFormat};
use valence::util::to_yaw_and_pitch;
use vek::{Mat3, Vec3};

pub fn main() -> ShutdownResult {
    env_logger::Builder::new()
        .filter_module("valence", LevelFilter::Trace)
        .parse_default_env()
        .init();

    valence::start_server(
        Game {
            player_count: AtomicUsize::new(0),
        },
        ServerState {
            player_list: None,
            cows: Vec::new(),
        },
    )
}

struct Game {
    player_count: AtomicUsize,
}

struct ServerState {
    player_list: Option<PlayerListId>,
    cows: Vec<EntityId>,
}

const MAX_PLAYERS: usize = 10;

const SPAWN_POS: BlockPos = BlockPos::new(0, 100, -25);

#[async_trait]
impl Config for Game {
    type ServerState = ServerState;
    type ClientState = EntityId;
    type EntityState = ();
    type WorldState = ();
    type ChunkState = ();
    type PlayerListState = ();

    fn max_connections(&self) -> usize {
        // We want status pings to be successful even if the server is full.
        MAX_PLAYERS + 64
    }

    async fn server_list_ping(
        &self,
        _server: &SharedServer<Self>,
        _remote_addr: SocketAddr,
        _protocol_version: i32,
    ) -> ServerListPing {
        ServerListPing::Respond {
            online_players: self.player_count.load(Ordering::SeqCst) as i32,
            max_players: MAX_PLAYERS as i32,
            description: "Hello Valence!".color(Color::AQUA),
            favicon_png: Some(include_bytes!("../assets/favicon.png")),
        }
    }

    fn init(&self, server: &mut Server<Self>) {
        let (world_id, world) = server.worlds.insert(DimensionId::default(), ());
        server.state.player_list = Some(server.player_lists.insert(()).0);

        let size = 5;
        for z in -size..size {
            for x in -size..size {
                world.chunks.insert([x, z], ());
            }
        }

        world.chunks.set_block_state(SPAWN_POS, BlockState::BEDROCK);

        server.state.cows.extend((0..200).map(|_| {
            let (id, e) = server.entities.insert(EntityKind::Cow, ());
            e.set_world(world_id);
            id
        }));
    }

    fn update(&self, server: &mut Server<Self>) {
        let (world_id, _) = server.worlds.iter_mut().next().expect("missing world");

        server.clients.retain(|_, client| {
            if client.created_this_tick() {
                if self
                    .player_count
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                        (count < MAX_PLAYERS).then_some(count + 1)
                    })
                    .is_err()
                {
                    client.disconnect("The server is full!".color(Color::RED));
                    return false;
                }

                match server
                    .entities
                    .insert_with_uuid(EntityKind::Player, client.uuid(), ())
                {
                    Some((id, _)) => client.state = id,
                    None => {
                        client.disconnect("Conflicting UUID");
                        return false;
                    }
                }

                client.spawn(world_id);
                client.set_flat(true);
                client.set_game_mode(GameMode::Creative);
                client.teleport(
                    [
                        SPAWN_POS.x as f64 + 0.5,
                        SPAWN_POS.y as f64 + 1.0,
                        SPAWN_POS.z as f64 + 0.5,
                    ],
                    0.0,
                    0.0,
                );
                client.set_player_list(server.state.player_list.clone());

                if let Some(id) = &server.state.player_list {
                    server.player_lists.get_mut(id).insert(
                        client.uuid(),
                        client.username(),
                        client.textures().cloned(),
                        client.game_mode(),
                        0,
                        None,
                    );
                }
            }

            if client.is_disconnected() {
                self.player_count.fetch_sub(1, Ordering::SeqCst);
                if let Some(id) = &server.state.player_list {
                    server.player_lists.get_mut(id).remove(client.uuid());
                }
                server.entities.remove(client.state);

                return false;
            }

            let entity = server
                .entities
                .get_mut(client.state)
                .expect("missing player entity");

            while client_event_boilerplate(client, entity).is_some() {}

            true
        });

        let time = server.shared.current_tick() as f64 / server.shared.tick_rate() as f64;

        let rot = Mat3::rotation_x(time * TAU * 0.1)
            .rotated_y(time * TAU * 0.2)
            .rotated_z(time * TAU * 0.3);

        let radius = 6.0 + ((time * TAU / 2.5).sin() + 1.0) / 2.0 * 10.0;

        let player_pos = server
            .clients
            .iter()
            .next()
            .map(|c| c.1.position())
            .unwrap_or_default();

        // TODO: remove hardcoded eye pos.
        let eye_pos = Vec3::new(player_pos.x, player_pos.y + 1.6, player_pos.z);

        for (cow_id, p) in server
            .state
            .cows
            .iter()
            .cloned()
            .zip(fibonacci_spiral(server.state.cows.len()))
        {
            let cow = server.entities.get_mut(cow_id).expect("missing cow");
            let rotated = p * rot;
            let transformed = rotated * radius + [0.5, SPAWN_POS.y as f64 + 2.0, 0.5];

            let yaw = f32::atan2(rotated.z as f32, rotated.x as f32).to_degrees() - 90.0;
            let (looking_yaw, looking_pitch) =
                to_yaw_and_pitch((eye_pos - transformed).normalized());

            cow.set_position(transformed);
            cow.set_yaw(yaw);
            cow.set_pitch(looking_pitch as f32);
            cow.set_head_yaw(looking_yaw as f32);
        }
    }
}

/// Distributes N points on the surface of a unit sphere.
fn fibonacci_spiral(n: usize) -> impl Iterator<Item = Vec3<f64>> {
    (0..n).map(move |i| {
        let golden_ratio = (1.0 + 5_f64.sqrt()) / 2.0;

        // Map to unit square
        let x = i as f64 / golden_ratio % 1.0;
        let y = i as f64 / n as f64;

        // Map from unit square to unit sphere.
        let theta = x * TAU;
        let phi = (1.0 - 2.0 * y).acos();
        Vec3::new(theta.cos() * phi.sin(), theta.sin() * phi.sin(), phi.cos())
    })
}

fn client_event_boilerplate(
    client: &mut Client<Game>,
    entity: &mut Entity<Game>,
) -> Option<ClientEvent> {
    let event = client.pop_event()?;

    match &event {
        ClientEvent::ChatMessage { .. } => {}
        ClientEvent::SettingsChanged {
            view_distance,
            main_hand,
            displayed_skin_parts,
            ..
        } => {
            client.set_view_distance(*view_distance);

            let player = client.player_mut();

            player.set_cape(displayed_skin_parts.cape());
            player.set_jacket(displayed_skin_parts.jacket());
            player.set_left_sleeve(displayed_skin_parts.left_sleeve());
            player.set_right_sleeve(displayed_skin_parts.right_sleeve());
            player.set_left_pants_leg(displayed_skin_parts.left_pants_leg());
            player.set_right_pants_leg(displayed_skin_parts.right_pants_leg());
            player.set_hat(displayed_skin_parts.hat());
            player.set_main_arm(*main_hand as u8);

            if let TrackedData::Player(player) = entity.data_mut() {
                player.set_cape(displayed_skin_parts.cape());
                player.set_jacket(displayed_skin_parts.jacket());
                player.set_left_sleeve(displayed_skin_parts.left_sleeve());
                player.set_right_sleeve(displayed_skin_parts.right_sleeve());
                player.set_left_pants_leg(displayed_skin_parts.left_pants_leg());
                player.set_right_pants_leg(displayed_skin_parts.right_pants_leg());
                player.set_hat(displayed_skin_parts.hat());
                player.set_main_arm(*main_hand as u8);
            }
        }
        ClientEvent::MovePosition {
            position,
            on_ground,
        } => {
            entity.set_position(*position);
            entity.set_on_ground(*on_ground);
        }
        ClientEvent::MovePositionAndRotation {
            position,
            yaw,
            pitch,
            on_ground,
        } => {
            entity.set_position(*position);
            entity.set_yaw(*yaw);
            entity.set_head_yaw(*yaw);
            entity.set_pitch(*pitch);
            entity.set_on_ground(*on_ground);
        }
        ClientEvent::MoveRotation {
            yaw,
            pitch,
            on_ground,
        } => {
            entity.set_yaw(*yaw);
            entity.set_head_yaw(*yaw);
            entity.set_pitch(*pitch);
            entity.set_on_ground(*on_ground);
        }
        ClientEvent::MoveOnGround { on_ground } => {
            entity.set_on_ground(*on_ground);
        }
        ClientEvent::MoveVehicle { .. } => {}
        ClientEvent::StartSneaking => {
            if let TrackedData::Player(player) = entity.data_mut() {
                if player.get_pose() == Pose::Standing {
                    player.set_pose(Pose::Sneaking);
                }
            }
        }
        ClientEvent::StopSneaking => {
            if let TrackedData::Player(player) = entity.data_mut() {
                if player.get_pose() == Pose::Sneaking {
                    player.set_pose(Pose::Standing);
                }
            }
        }
        ClientEvent::StartSprinting => {
            if let TrackedData::Player(player) = entity.data_mut() {
                player.set_sprinting(true);
            }
        }
        ClientEvent::StopSprinting => {
            if let TrackedData::Player(player) = entity.data_mut() {
                player.set_sprinting(false);
            }
        }
        ClientEvent::StartJumpWithHorse { .. } => {}
        ClientEvent::StopJumpWithHorse => {}
        ClientEvent::LeaveBed => {}
        ClientEvent::OpenHorseInventory => {}
        ClientEvent::StartFlyingWithElytra => {}
        ClientEvent::ArmSwing(hand) => {
            entity.push_event(match hand {
                Hand::Main => EntityEvent::SwingMainHand,
                Hand::Off => EntityEvent::SwingOffHand,
            });
        }
        ClientEvent::InteractWithEntity { .. } => {}
        ClientEvent::SteerBoat { .. } => {}
        ClientEvent::Digging { .. } => {}
    }

    entity.set_world(client.world());

    Some(event)
}
