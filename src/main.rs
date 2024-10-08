#![windows_subsystem = "windows"]
use bevy::{
    prelude::*,
    render::{
        settings::{Backends, RenderCreation, WgpuSettings}, RenderPlugin
    },
    utils::HashMap,
    window::{PresentMode, WindowFocused, WindowResized},
};
use bevy_web_asset::WebAssetPlugin;
use emotes::{get_seventv_emotes, update_emote_meta};
use log::info;
use std::time::{Duration, Instant};
use tokio::{sync::mpsc, time::sleep};
use twitch_irc::{
    login::StaticLoginCredentials, ClientConfig, SecureTCPTransport, TwitchIRCClient,
};
use vleue_kinetoscope::AnimatedImagePlugin;
use env_logger::Env;

mod types;
use types::*;

mod users;
use users::{despawn_users, move_users, spawn_user};

mod messages;
use messages::{despawn_messages, display_message};

mod emotes;

mod config;
use config::{Config, load_config};

#[tokio::main]
async fn main() {
    let config = load_config("config.ini");
    let channel_id = config.channel_id.clone(); // TODO: Can I not double clone this?
    let setup_with_channel_id = move |commands: Commands,
                                 windows: Query<&mut Window>,
                                 emotes_rec: ResMut<EmoteStorage>,
                                 app_state: ResMut<AppState>| {
        setup(commands, windows, emotes_rec, app_state, config.scale, channel_id.clone())
    };

    let env = Env::default()
        .filter_or("LOG_LEVEL", "info")
        .write_style_or("LOG_STYLE", "always");

    env_logger::init_from_env(env);

    // Create a channel to communicate between Twitch client and Bevy
    let (tx, rx) = mpsc::channel::<TwitchMessage>(100);

    let channel_name = config.channel_name.clone();
    // Start Twitch IRC client in a separate async task
    tokio::spawn(async move {
        start_twitch_client(tx, channel_name).await;
    });

    // Set up Wgpu settings
    let wgpu_settings = WgpuSettings {
        backends: Some(Backends::VULKAN),
        ..Default::default()
    };

    // Run Bevy application
    App::new()
        .insert_resource(config)
        .insert_resource(ClearColor(Color::NONE))
        .insert_resource(TwitchReceiver { receiver: rx })
        .insert_resource(EmoteStorage {
            all: HashMap::new(),
            loaded: HashMap::new(),
        })
        .insert_resource(AppState {
            active_users: HashMap::new(),
            program_state: ProgramState::Loading,
        })
        .add_plugins(WebAssetPlugin)
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "Transparent Window".to_string(),
                        transparent: true,
                        decorations: false,
                        present_mode: PresentMode::Mailbox,
                        window_level: bevy::window::WindowLevel::AlwaysOnTop,
                        ..default()
                    }),
                    ..default()
                })
                .set(RenderPlugin {
                    render_creation: RenderCreation::Automatic(wgpu_settings),
                    synchronous_pipeline_compilation: false,
                })
        )
        .add_plugins(AnimatedImagePlugin)
        .add_systems(Startup, setup_with_channel_id)
        .add_systems(
            Update,
            (
                move_users,
                despawn_users,
                despawn_messages,
                handle_twitch_messages,
                handle_window_events,
                adjust_sprite_scale_system,
            ),
        )
        .run();
}

// Set up the camera and window
fn setup(
    mut commands: Commands,
    mut windows: Query<&mut Window>,
    mut emotes_rec: ResMut<EmoteStorage>,
    mut app_state: ResMut<AppState>,
    scale_factor: f32,
    channel_id: String
) {
    commands.spawn(Camera2dBundle::default());
    let mut window: Mut<'_, Window> = windows.single_mut();
    window.resolution.set_scale_factor_override(Some(scale_factor));
    window.cursor.hit_test = false;
    window.set_maximized(true);

    setup_seventv_emotes(&mut emotes_rec, channel_id);

    app_state.program_state = ProgramState::Running;
}

fn setup_seventv_emotes(emotes_rec: &mut ResMut<EmoteStorage>, channel_id: String) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let emotes = rt.block_on(async { get_seventv_emotes(channel_id).await });

    emotes_rec.all.extend(emotes);
}

async fn start_twitch_client(tx: mpsc::Sender<TwitchMessage>, channel: String) {
    let config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());

    let (mut incoming_messages, client) =
        TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

    client.join(channel).unwrap();

    sleep(Duration::from_millis(2000)).await;

    let mut seen_emotes: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Listen to incoming Twitch messages and send them to Bevy via the channel
    while let Some(message) = incoming_messages.recv().await {
        if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
            info!("{}: {}", msg.sender.name, msg.message_text);
            let mut twitch_message = TwitchMessage {
                user: msg.sender.name.clone(),
                message: msg.message_text.clone(),
                emotes: msg.emotes.into_iter().map(|emote| emote.into()).collect(),
            };

            let mut new_emotes: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for emote in twitch_message
                .emotes
                .iter_mut()
                .filter(|emote| !seen_emotes.contains(&emote.name))
            {
                update_emote_meta(emote).await;
                new_emotes.insert(emote.name.clone());
            }
            seen_emotes.extend(new_emotes);
            tx.send(twitch_message).await.unwrap(); // Use the cloned tx value
        }
    }
}

/// System to handle incoming Twitch messages
fn handle_twitch_messages(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut emote_rec: ResMut<EmoteStorage>,
    query: Query<&Camera>,
    mut app_state: ResMut<AppState>,
    config: Res<Config>,
    mut twitch_receiver: ResMut<TwitchReceiver>,
) {
    while let Ok(twitch_message) = twitch_receiver.receiver.try_recv() {
        // Add any new emotes to the storage
        for emote in twitch_message.emotes.iter() {
            emote_rec
                .all
                .entry(emote.name.clone())
                .or_insert(emote.clone());
        }
        // Check if the user already exists
        if let Some(user) = app_state.active_users.get_mut(&twitch_message.user) {
            // Update the user's last message time and display the message
            display_message(
                &mut commands,
                &asset_server,
                &mut emote_rec,
                &config,
                user.entity,
                twitch_message.message,
            );
            // user.last_message = Some(message);
            user.last_message_time = Instant::now();
        } else {
            // Add new user and spawn their avatar
            let rect = query.single().logical_viewport_rect().unwrap();
            let entity = spawn_user(&mut commands, &asset_server, &twitch_message, &config, rect);
            display_message(
                &mut commands,
                &asset_server,
                &mut emote_rec,
                &config,
                entity,
                twitch_message.message,
            );
            app_state.active_users.insert(
                twitch_message.user.clone(),
                User {
                    entity,
                    _name: twitch_message.user.clone(),
                    last_message_time: Instant::now(),
                },
            );
        }
    }
}

fn handle_window_events(
    window_moved_events: EventReader<WindowMoved>,
    window_resized_events: EventReader<WindowResized>,
    window_focused_events: EventReader<WindowFocused>,
    windows: Query<&mut Window>,
    mut avatar_query: Query<&mut Transform, With<UserMarker>>,
) {
    // Check if any relevant window events have occurred
    if !window_moved_events.is_empty()
        || !window_resized_events.is_empty()
        || !window_focused_events.is_empty()
    {
        // Get the primary window
        if let Ok(window) = windows.get_single() {
            let rect = window.physical_size();
            for mut transform in avatar_query.iter_mut() {
                transform.translation.x = transform
                    .translation
                    .x
                    .max(-(rect.x as f32 / 2.0))
                    .min(rect.x as f32 / 2.0);
                transform.translation.y = -(rect.y as f32 / 2.0) + 25.0;
            }
        }
    }
}

fn adjust_sprite_scale_system(
    mut commands: Commands,
    mut query: Query<(Entity, &Handle<Image>, &mut Sprite, &mut Visibility), With<AdjustScale>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (entity, texture_handle, mut sprite, mut visibility) in query.iter_mut() {
        if let Some(image) = images.get_mut(texture_handle) {
            let texture_height = image.texture_descriptor.size.height as f32;
            let scale_factor = 46.0 / texture_height;
            
            // Modify sprite custom size and make visible
            sprite.custom_size.replace(Vec2::new(image.texture_descriptor.size.width as f32 * scale_factor, texture_height * scale_factor));
            *visibility = Visibility::Visible;

            // Remove the marker component
            commands.entity(entity).remove::<AdjustScale>();
        }
    }
}