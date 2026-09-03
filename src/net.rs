use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{SystemTime, UNIX_EPOCH},
};

use bevy::{prelude::*, winit::WinitSettings};
use bevy_inspector_egui::bevy_egui::{egui, EguiContexts};
use bevy_rapier2d::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::{
    renet::{
        transport::{
            ClientAuthentication, NetcodeClientTransport, NetcodeServerTransport,
            ServerAuthentication, ServerConfig,
        },
        ConnectionConfig, RenetClient, RenetServer,
    },
    RenetChannelsExt, RepliconRenetPlugins,
};
use serde::{Deserialize, Serialize};

use crate::{player, player::Team, GameState};

// WHY: v1 favors a typed LAN address over discovery so transport work stays scoped.
pub const DEFAULT_PORT: u16 = 5223;
// Bump whenever the replicated schema below changes, to avoid stale-client confusion.
const PROTOCOL_ID: u64 = 3;

pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RepliconRenetPlugins)
            .insert_resource(WinitSettings {
                focused_mode: bevy::winit::UpdateMode::Continuous,
                unfocused_mode: bevy::winit::UpdateMode::Continuous,
            })
            .replicate::<Transform>()
            .replicate::<NetPlayer>()
            .replicate::<Team>()
            .replicate::<player::Queen>()
            .add_client_event::<PlayerInput>(ChannelKind::Unreliable)
            .add_client_event::<TeamSelection>(ChannelKind::Ordered)
            .add_systems(Update, net_setup_ui.run_if(in_state(GameState::NetSetup)))
            .add_systems(
                Update,
                (send_team_selection, enter_join_on_client_connect)
                    .chain()
                    .run_if(client_just_connected),
            )
            .add_systems(
                Update,
                (
                    apply_team_selection.run_if(has_authority),
                    (attach_net_physics, track_net_grounded, apply_net_input)
                        .chain()
                        .run_if(has_authority),
                    send_local_input.run_if(in_state(GameState::Join)),
                    render_net_players,
                ),
            );
    }
}

/// A networked queen/worker player entity. The host attaches real Rapier
/// physics via `attach_net_physics`; clients only ever receive the
/// replicated `Transform` and render it — inserting physics components off
/// the host would run an independent local simulation that fights the
/// replicated position. Queen-ness is carried by the replicated
/// `player::Queen` marker: the first player to join a team is the queen,
/// every later joiner on that team is a worker.
#[derive(Component, Serialize, Deserialize, Clone, Copy)]
pub struct NetPlayer;

/// Associates authoritative input with the player it is allowed to move.
#[derive(Component)]
struct Owner(ClientId);

/// Host-local ground state for a net player, refreshed each frame from
/// `ContactForceEvent`. Not replicated: only the host runs physics.
#[derive(Component, Default)]
struct NetGrounded(bool);

#[derive(Resource)]
struct RequestedTeam(Team);

#[derive(Event, Serialize, Deserialize, Debug, Clone, Copy)]
struct TeamSelection {
    team: Team,
}

#[derive(Event, Serialize, Deserialize, Debug, Default, Clone, Copy)]
struct PlayerInput {
    move_x: f32,
    /// Edge-triggered primary action: ground jump for a worker, a wing flap
    /// for a queen (ignored server-side for a worker either way).
    jump: bool,
    /// Held state, meaningful only for a queen: dive gravity/speed while
    /// held, ignored server-side for a worker.
    dive: bool,
}

/// Marks a replicated entity that has already had its local-only render
/// components attached, so `render_net_players` only does it once.
#[derive(Component)]
struct NetSprite;

fn net_setup_ui(
    mut contexts: EguiContexts,
    mut commands: Commands,
    mut next_state: ResMut<NextState<GameState>>,
    mut join_addr: Local<String>,
    mut selected_team: Local<Option<Team>>,
    channels: Res<RepliconChannels>,
) {
    if join_addr.is_empty() {
        *join_addr = format!("127.0.0.1:{DEFAULT_PORT}");
    }
    egui::Window::new("Networked Multiplayer").show(contexts.ctx_mut(), |ui| {
        ui.label("Use the same WiFi/LAN. Clients type the host computer's local IP:port.");
        ui.horizontal(|ui| {
            ui.label("Team:");
            ui.selectable_value(&mut *selected_team, Some(Team::Yellow), "Yellow");
            ui.selectable_value(&mut *selected_team, Some(Team::Purple), "Purple");
        });

        let team = *selected_team;
        if ui
            .add_enabled(team.is_some(), egui::Button::new("Host"))
            .clicked()
        {
            if let Some(team) = team {
                host(&mut commands, &channels, team);
                next_state.set(GameState::Join);
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Join:");
            ui.text_edit_singleline(&mut *join_addr);
            if ui
                .add_enabled(team.is_some(), egui::Button::new("Connect"))
                .clicked()
            {
                if let Some(team) = team {
                    match join_addr.parse::<SocketAddr>() {
                        Ok(addr) => join(&mut commands, &channels, addr, team),
                        Err(err) => warn!("could not parse address {}: {err}", *join_addr),
                    }
                }
            }
        });
        if team.is_none() {
            ui.label("Choose a team to enable Host and Connect.");
        }
    });
}

fn host(commands: &mut Commands, channels: &RepliconChannels, team: Team) {
    let server = RenetServer::new(ConnectionConfig {
        server_channels_config: channels.get_server_configs(),
        client_channels_config: channels.get_client_configs(),
        ..Default::default()
    });

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_PORT))
        .expect("failed to bind host UDP socket");
    let server_config = ServerConfig {
        current_time: SystemTime::now().duration_since(UNIX_EPOCH).unwrap(),
        max_clients: 10,
        protocol_id: PROTOCOL_ID,
        authentication: ServerAuthentication::Unsecure,
        public_addresses: vec![socket.local_addr().unwrap()],
    };
    let transport =
        NetcodeServerTransport::new(server_config, socket).expect("failed to start host transport");

    commands.insert_resource(server);
    commands.insert_resource(transport);

    // Nothing has spawned yet, so the host is always the first (and thus the
    // queen) for its chosen team.
    spawn_net_player(commands, ClientId::SERVER, team, true);
}

fn join(commands: &mut Commands, channels: &RepliconChannels, server_addr: SocketAddr, team: Team) {
    let client = RenetClient::new(ConnectionConfig {
        server_channels_config: channels.get_server_configs(),
        client_channels_config: channels.get_client_configs(),
        ..Default::default()
    });

    let client_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let socket =
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("failed to bind client UDP socket");
    let current_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let authentication = ClientAuthentication::Unsecure {
        client_id,
        protocol_id: PROTOCOL_ID,
        server_addr,
        user_data: None,
    };
    let transport = NetcodeClientTransport::new(current_time, authentication, socket)
        .expect("failed to start client transport");

    commands.insert_resource(client);
    commands.insert_resource(transport);
    commands.insert_resource(RequestedTeam(team));
}

fn send_team_selection(requested_team: Res<RequestedTeam>, mut events: EventWriter<TeamSelection>) {
    events.send(TeamSelection {
        team: requested_team.0,
    });
}

fn enter_join_on_client_connect(mut next_state: ResMut<NextState<GameState>>) {
    next_state.set(GameState::Join);
}

fn apply_team_selection(
    mut commands: Commands,
    mut events: EventReader<FromClient<TeamSelection>>,
    players: Query<&Owner, With<NetPlayer>>,
    queens: Query<&Team, (With<NetPlayer>, With<player::Queen>)>,
) {
    let mut assigned_clients: HashSet<_> = players.iter().map(|owner| owner.0).collect();
    for FromClient { client_id, event } in events.read() {
        if assigned_clients.insert(*client_id) {
            let is_queen = !queens.iter().any(|&queen_team| queen_team == event.team);
            spawn_net_player(&mut commands, *client_id, event.team, is_queen);
        }
    }
}

fn spawn_net_player(commands: &mut Commands, client_id: ClientId, team: Team, is_queen: bool) {
    let x = match team {
        Team::Yellow => -200.0,
        Team::Purple => 200.0,
    };
    let mut player = commands.spawn((
        NetPlayer,
        team,
        Transform::from_xyz(x, 0.0, 5.0),
        Owner(client_id),
        Replicated,
    ));
    if is_queen {
        player.insert(player::Queen);
    }
}

/// Blueprint pattern: replicated entities arrive with only data components
/// (Transform/NetPlayer/Team). Attach the host-only physics bundle once,
/// keyed on `Without<RigidBody>` so it never re-runs for an entity that
/// already has it. Gated to `has_authority` at the call site, so this never
/// executes on a client and a client-side entity never gets local physics.
fn attach_net_physics(
    mut commands: Commands,
    new_players: Query<(Entity, Has<player::Queen>), (With<NetPlayer>, Without<RigidBody>)>,
) {
    for (entity, is_queen) in &new_players {
        let (width, height) = if is_queen {
            (player::QUEEN_RENDER_WIDTH, player::QUEEN_RENDER_HEIGHT)
        } else {
            (player::WORKER_RENDER_WIDTH, player::WORKER_RENDER_HEIGHT)
        };
        commands.entity(entity).insert((
            RigidBody::Dynamic,
            GravityScale(player::PLAYER_GRAVITY_SCALE),
            Collider::cuboid(
                width / 2.0 * player::PLAYER_COLLIDER_WIDTH_MULTIPLIER,
                height / 2.0,
            ),
            Velocity::default(),
            ExternalImpulse::default(),
            LockedAxes::ROTATION_LOCKED,
            Friction {
                coefficient: 0.0,
                combine_rule: CoefficientCombineRule::Min,
            },
            ActiveEvents::all(),
            Ccd::enabled(),
            NetGrounded::default(),
        ));
    }
}

/// Host-only ground detection for net players, mirroring
/// `player::check_if_players_on_ground`.
fn track_net_grounded(
    mut contact_force_events: EventReader<ContactForceEvent>,
    mut players: Query<&mut NetGrounded>,
) {
    for mut grounded in &mut players {
        grounded.0 = false;
    }
    for event in contact_force_events.read() {
        if let Ok(mut grounded) = players.get_mut(event.collider1) {
            if event.max_force_direction.y != 0.0 {
                grounded.0 = true;
            }
        }
        if let Ok(mut grounded) = players.get_mut(event.collider2) {
            if event.max_force_direction.y != 0.0 {
                grounded.0 = true;
            }
        }
    }
}

/// Host-only authoritative physics step: applies a client's input as a real
/// impulse against the host's Rapier simulation, matching local play's
/// ground/air movement, friction, and jump/fly/dive impulses. A worker jumps
/// off the ground; a queen (`Has<Queen>`) flaps to fly and can dive for a
/// faster, gravity-boosted descent — mirroring `player::fly`/`player::dive`.
fn apply_net_input(
    mut events: EventReader<FromClient<PlayerInput>>,
    mut players: Query<
        (
            &Owner,
            &mut ExternalImpulse,
            &mut Velocity,
            &mut GravityScale,
            &NetGrounded,
            Has<player::Queen>,
        ),
        With<NetPlayer>,
    >,
    time: Res<Time>,
) {
    for FromClient { client_id, event } in events.read() {
        for (owner, mut impulse, mut velocity, mut gravity, grounded, is_queen) in &mut players {
            if owner.0 != *client_id {
                continue;
            }

            if event.move_x != 0.0 && !(event.dive && grounded.0) {
                let movement_impulse = if grounded.0 {
                    player::PLAYER_MOVEMENT_IMPULSE_GROUND
                } else {
                    player::PLAYER_MOVEMENT_IMPULSE_AIR
                };
                impulse.impulse.x += event.move_x * movement_impulse * time.delta_seconds();
            } else if velocity.linvel.x.abs() < player::PLAYER_MIN_VELOCITY_X {
                velocity.linvel.x = 0.0;
            }

            if grounded.0 {
                impulse.impulse.x -=
                    velocity.linvel.x * player::PLAYER_FRICTION_GROUND * time.delta_seconds();
            }

            if is_queen {
                gravity.0 = if event.dive {
                    player::DIVE_GRAVITY_SCALE
                } else {
                    player::PLAYER_GRAVITY_SCALE
                };
                if event.jump && !event.dive {
                    impulse.impulse.y += player::PLAYER_FLY_IMPULSE;
                }
            } else if event.jump && grounded.0 {
                impulse.impulse.y += player::PLAYER_JUMP_IMPULSE;
            }

            velocity.linvel.x = velocity.linvel.x.clamp(
                -player::PLAYER_MAX_VELOCITY_X,
                player::PLAYER_MAX_VELOCITY_X,
            );
            velocity.linvel.y = velocity.linvel.y.clamp(
                if is_queen && event.dive {
                    -player::PLAYER_MAX_DIVE_SPEED
                } else {
                    -player::PLAYER_MAX_FALL_SPEED
                },
                if is_queen {
                    player::PLAYER_MAX_RISE_SPEED
                } else {
                    f32::MAX
                },
            );
        }
    }
}

fn send_local_input(keys: Res<ButtonInput<KeyCode>>, mut events: EventWriter<PlayerInput>) {
    let mut move_x = 0.0;
    if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
        move_x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
        move_x += 1.0;
    }
    // Edge-triggered: sampling `pressed` here would re-fire the jump impulse
    // every frame the key is held, since the impulse is applied per received
    // event rather than tracked as a discrete action state.
    let jump = keys.just_pressed(KeyCode::Space)
        || keys.just_pressed(KeyCode::ArrowUp)
        || keys.just_pressed(KeyCode::KeyW);
    // Held, not edge-triggered: a queen dives for as long as this is down.
    // Sent unconditionally; the host ignores it for a worker.
    let dive = keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown);
    events.send(PlayerInput { move_x, jump, dive });
}

/// Blueprint pattern: replicated entities arrive with only data components
/// (Transform/NetPlayer/Team). Attach the local, non-replicated render
/// components once, without touching the already-replicated Transform —
/// inserting a bundle with its own `transform` field would clobber it.
fn render_net_players(
    server: Res<AssetServer>,
    mut atlas_layouts: ResMut<Assets<TextureAtlasLayout>>,
    mut commands: Commands,
    new_players: Query<(Entity, &Team, Has<player::Queen>), (With<NetPlayer>, Without<NetSprite>)>,
) {
    for (entity, team, is_queen) in &new_players {
        let texture: Handle<Image> = server.load(player::get_spritesheet(*team, is_queen));
        let layout = TextureAtlasLayout::from_grid(
            Vec2::new(player::SPRITE_TILE_WIDTH, player::SPRITE_TILE_HEIGHT),
            player::SPRITESHEET_COLS,
            player::SPRITESHEET_ROWS,
            None,
            None,
        );
        let layout_handle = atlas_layouts.add(layout);
        let (rect, width, height) = if is_queen {
            (
                player::QUEEN_RECT,
                player::QUEEN_RENDER_WIDTH,
                player::QUEEN_RENDER_HEIGHT,
            )
        } else {
            (
                player::WORKER_RECT,
                player::WORKER_RENDER_WIDTH,
                player::WORKER_RENDER_HEIGHT,
            )
        };
        commands.entity(entity).insert((
            texture,
            TextureAtlas {
                layout: layout_handle,
                index: player::SPRITE_IDX_STAND,
            },
            Sprite {
                rect: Some(rect),
                custom_size: Some(Vec2::new(width, height)),
                ..default()
            },
            GlobalTransform::default(),
            Visibility::default(),
            InheritedVisibility::default(),
            ViewVisibility::default(),
            NetSprite,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_replicon::test_app::ServerTestAppExt;

    #[test]
    fn server_uses_first_team_selected_by_client() {
        let mut server = App::new();
        let mut client = App::new();
        for app in [&mut server, &mut client] {
            app.add_plugins((MinimalPlugins, RepliconPlugins))
                .add_client_event::<TeamSelection>(ChannelKind::Ordered);
        }
        server.add_systems(Update, apply_team_selection);
        client
            .insert_resource(RequestedTeam(Team::Purple))
            .add_systems(Update, send_team_selection.run_if(client_just_connected));
        server.connect_client(&mut client);

        server.exchange_with_client(&mut client);
        server.update();
        send_selection(&mut server, &mut client, Team::Yellow);

        let players: Vec<_> = server
            .world
            .query::<(&Owner, &Team)>()
            .iter(&server.world)
            .map(|(owner, team)| (owner.0, *team))
            .collect();
        assert_eq!(players.len(), 1);
        assert_ne!(players[0].0, ClientId::SERVER);
        assert_eq!(players[0].1, Team::Purple);
    }

    fn send_selection(server: &mut App, client: &mut App, team: Team) {
        client.world.send_event(TeamSelection { team });
        client.update();
        server.exchange_with_client(client);
        server.update();
    }
}
