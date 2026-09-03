use std::{
    collections::HashSet,
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

// WHY: v1 favors a typed LAN address over discovery so transport work stays scoped.
pub const DEFAULT_PORT: u16 = 5223;
// Bump whenever the replicated schema below changes, to avoid stale-client confusion.
const PROTOCOL_ID: u64 = 2;

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
                    apply_input.run_if(has_authority),
                    send_local_input.run_if(in_state(GameState::Join)),
                    render_net_players,
                ),
            );
    }
}

/// Keeps transport verification independent of the existing physics-heavy player bundle.
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

/// Associates authoritative input with the player it is allowed to move.
#[derive(Component)]
struct Owner(ClientId);

#[derive(Resource)]
struct RequestedTeam(NetTeam);

#[derive(Event, Serialize, Deserialize, Debug, Clone, Copy)]
struct TeamSelection {
    team: NetTeam,
}

#[derive(Event, Serialize, Deserialize, Debug, Default, Clone, Copy)]
struct PlayerInput {
    move_x: f32,
    jump: bool,
}

#[derive(Component)]
struct NetSprite;

fn net_setup_ui(
    mut contexts: EguiContexts,
    mut commands: Commands,
    mut next_state: ResMut<NextState<GameState>>,
    mut join_addr: Local<String>,
    mut selected_team: Local<Option<NetTeam>>,
    channels: Res<RepliconChannels>,
) {
    if join_addr.is_empty() {
        *join_addr = format!("127.0.0.1:{DEFAULT_PORT}");
    }
    egui::Window::new("Networked Multiplayer").show(contexts.ctx_mut(), |ui| {
        ui.label("Use the same WiFi/LAN. Clients type the host computer's local IP:port.");
        ui.horizontal(|ui| {
            ui.label("Team:");
            ui.selectable_value(&mut *selected_team, Some(NetTeam::Yellow), "Yellow");
            ui.selectable_value(&mut *selected_team, Some(NetTeam::Purple), "Purple");
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

fn host(commands: &mut Commands, channels: &RepliconChannels, team: NetTeam) {
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

    spawn_net_player(commands, ClientId::SERVER, team);
}

fn join(
    commands: &mut Commands,
    channels: &RepliconChannels,
    server_addr: SocketAddr,
    team: NetTeam,
) {
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
) {
    let mut assigned_clients: HashSet<_> = players.iter().map(|owner| owner.0).collect();
    for FromClient { client_id, event } in events.read() {
        if assigned_clients.insert(*client_id) {
            spawn_net_player(&mut commands, *client_id, event.team);
        }
    }
}

fn spawn_net_player(commands: &mut Commands, client_id: ClientId, team: NetTeam) {
    let x = match team {
        NetTeam::Yellow => -200.0,
        NetTeam::Purple => 200.0,
    };
    commands.spawn((
        NetPlayer,
        team,
        Transform::from_xyz(x, 0.0, 5.0),
        Owner(client_id),
        Replicated,
    ));
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
    let jump = keys.pressed(KeyCode::Space)
        || keys.pressed(KeyCode::ArrowUp)
        || keys.pressed(KeyCode::KeyW);
    events.send(PlayerInput { move_x, jump });
}

/// INVARIANT: Add only client-local rendering state; the replicated `Transform` stays authoritative.
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
            .insert_resource(RequestedTeam(NetTeam::Purple))
            .add_systems(Update, send_team_selection.run_if(client_just_connected));
        server.connect_client(&mut client);

        server.exchange_with_client(&mut client);
        server.update();
        send_selection(&mut server, &mut client, NetTeam::Yellow);

        let players: Vec<_> = server
            .world
            .query::<(&Owner, &NetTeam)>()
            .iter(&server.world)
            .map(|(owner, team)| (owner.0, *team))
            .collect();
        assert_eq!(players.len(), 1);
        assert_ne!(players[0].0, ClientId::SERVER);
        assert_eq!(players[0].1, NetTeam::Purple);
    }

    fn send_selection(server: &mut App, client: &mut App, team: NetTeam) {
        client.world.send_event(TeamSelection { team });
        client.update();
        server.exchange_with_client(client);
        server.update();
    }
}
