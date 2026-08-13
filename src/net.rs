use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{SystemTime, UNIX_EPOCH},
};

use bevy::{prelude::*, winit::WinitSettings};
use bevy_inspector_egui::bevy_egui::{egui, EguiContexts};
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

use crate::GameState;

// Fixed default port for the "typed IP:port" connection flow (no LAN auto-discovery in v1).
pub const DEFAULT_PORT: u16 = 5223;
// Bump whenever the replicated schema below changes, to avoid stale-client confusion.
const PROTOCOL_ID: u64 = 1;

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
            .replicate::<NetTeam>()
            .add_client_event::<PlayerInput>(ChannelKind::Unreliable)
            .add_systems(
                Update,
                net_setup_ui.run_if(in_state(GameState::NetSetup)),
            )
            .add_systems(Update, enter_join_on_client_connect.run_if(client_just_connected))
            .add_systems(
                Update,
                (
                    handle_client_connect.run_if(has_authority),
                    apply_input.run_if(has_authority),
                    send_local_input,
                    render_net_players,
                ),
            );
    }
}

/// Marks a minimal networked "walking skeleton" player entity — a placeholder
/// colored rectangle, not the real queen/worker sprite. See the networked
/// multiplayer goal's progress log for why this is scoped down.
#[derive(Component, Serialize, Deserialize, Clone, Copy)]
pub struct NetPlayer;

#[derive(Component, Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum NetTeam {
    Yellow,
    Purple,
}

impl NetTeam {
    fn color(&self) -> Color {
        match self {
            NetTeam::Yellow => Color::rgb(1.0, 0.773, 0.0),
            NetTeam::Purple => Color::rgb(0.435, 0.0, 1.0),
        }
    }
}

/// Maps a server-side entity to the client that controls it.
/// `ClientId::SERVER` marks the host's own locally-controlled entity.
#[derive(Component)]
struct Owner(ClientId);

#[derive(Event, Serialize, Deserialize, Debug, Default, Clone, Copy)]
struct PlayerInput {
    move_x: f32,
    jump: bool,
}

/// Marks a client-side entity that has already had its render bundle attached,
/// so `render_net_players` only does it once per replicated entity.
#[derive(Component)]
struct NetSprite;

fn net_setup_ui(
    mut contexts: EguiContexts,
    mut commands: Commands,
    mut next_state: ResMut<NextState<GameState>>,
    mut join_addr: Local<String>,
    channels: Res<RepliconChannels>,
) {
    if join_addr.is_empty() {
        *join_addr = format!("127.0.0.1:{DEFAULT_PORT}");
    }
    egui::Window::new("Networked Multiplayer").show(contexts.ctx_mut(), |ui| {
        ui.label("Host a game, or join one by typing the host's IP:port.");
        if ui.button("Host").clicked() {
            host(&mut commands, &channels);
            next_state.set(GameState::Join);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Join:");
            ui.text_edit_singleline(&mut *join_addr);
            if ui.button("Connect").clicked() {
                match join_addr.parse::<SocketAddr>() {
                    Ok(addr) => join(&mut commands, &channels, addr),
                    Err(err) => warn!("could not parse address {}: {err}", *join_addr),
                }
            }
        });
    });
}

fn host(commands: &mut Commands, channels: &RepliconChannels) {
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
    let transport = NetcodeServerTransport::new(server_config, socket)
        .expect("failed to start host transport");

    commands.insert_resource(server);
    commands.insert_resource(transport);

    // The host's own player is a normal server-side spawn, not a loopback client.
    commands.spawn((
        NetPlayer,
        NetTeam::Yellow,
        Transform::from_xyz(-200.0, 0.0, 5.0),
        Owner(ClientId::SERVER),
        Replicated,
    ));
}

fn join(commands: &mut Commands, channels: &RepliconChannels, server_addr: SocketAddr) {
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
}

fn enter_join_on_client_connect(mut next_state: ResMut<NextState<GameState>>) {
    next_state.set(GameState::Join);
}

fn handle_client_connect(mut commands: Commands, mut server_events: EventReader<ServerEvent>) {
    for event in server_events.read() {
        if let ServerEvent::ClientConnected { client_id } = event {
            commands.spawn((
                NetPlayer,
                NetTeam::Purple,
                Transform::from_xyz(200.0, 0.0, 5.0),
                Owner(*client_id),
                Replicated,
            ));
        }
    }
}

fn apply_input(
    mut events: EventReader<FromClient<PlayerInput>>,
    mut players: Query<(&Owner, &mut Transform), With<NetPlayer>>,
    time: Res<Time>,
) {
    for FromClient { client_id, event } in events.read() {
        for (owner, mut transform) in &mut players {
            if owner.0 != *client_id {
                continue;
            }
            transform.translation.x += event.move_x * 200.0 * time.delta_seconds();
            let target_y = if event.jump { 120.0 } else { 0.0 };
            let step = 300.0 * time.delta_seconds();
            transform.translation.y = if transform.translation.y < target_y {
                (transform.translation.y + step).min(target_y)
            } else {
                (transform.translation.y - step).max(target_y)
            };
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
    let jump =
        keys.pressed(KeyCode::Space) || keys.pressed(KeyCode::ArrowUp) || keys.pressed(KeyCode::KeyW);
    events.send(PlayerInput { move_x, jump });
}

/// Blueprint pattern: replicated entities arrive with only data components
/// (Transform/NetPlayer/NetTeam). Attach the local, non-replicated render
/// bundle pieces once, without touching the already-replicated Transform.
fn render_net_players(
    mut commands: Commands,
    new_players: Query<(Entity, &NetTeam), (With<NetPlayer>, Without<NetSprite>)>,
) {
    for (entity, team) in &new_players {
        commands.entity(entity).insert((
            Sprite {
                color: team.color(),
                custom_size: Some(Vec2::new(40.0, 40.0)),
                ..default()
            },
            Handle::<Image>::default(),
            GlobalTransform::default(),
            Visibility::default(),
            InheritedVisibility::default(),
            ViewVisibility::default(),
            NetSprite,
        ));
    }
}
