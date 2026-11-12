//! The 3D arena and the match replay.
//!
//! The server simulates a whole match up front and stores one `frame` row per
//! tick. Here we work out which tick should be on screen right now, blend the
//! two neighbouring frames and move one sphere per bot accordingly.

use bevy::prelude::*;
use promptnite_bindings::*;

use crate::{net::Conn, theme::PALETTE, ui::Screen};

/// What is on screen this frame. Written here, read by the UI for labels.
#[derive(Resource, Default)]
pub struct Replay {
    pub bots: Vec<BotView>,
    /// Seconds since we saw the current phase begin.
    pub phase_elapsed: f32,
    /// `(room, round, phase)` we last saw, and the app time when it changed.
    seen: Option<(u32, u32, Phase, f64)>,
}

/// Primary key of a `frame` row. Must match `frame_key` in the server module.
fn frame_key(room_id: u32, tick: u32) -> u64 {
    (room_id as u64) << 32 | tick as u64
}

pub struct BotView {
    pub id: u32,
    /// World position of the bot's centre.
    pub position: Vec3,
    pub radius: f32,
    pub hp: i32,
    pub action: BotAction,
}

/// The camera that looks at the arena (as opposed to the UI camera).
#[derive(Component)]
pub struct ArenaCamera;

/// Tilted view from behind edge `edge` of the arena, looking across it.
/// The edge table must match `edge_direction` in `sim/src/rules.rs`.
fn camera_transform(edge: u32) -> Transform {
    let outward = outward(edge);
    Transform::from_translation(outward * 19.0 + Vec3::Y * 21.0).looking_at(outward, Vec3::Y)
}

/// Out from the centre towards edge `edge`.
fn outward(edge: u32) -> Vec3 {
    // Sim (x, y) is the ground plane; Bevy's up axis is Y.
    match edge % 4 {
        0 => Vec3::Z,
        1 => Vec3::NEG_Z,
        2 => Vec3::X,
        _ => Vec3::NEG_X,
    }
}

/// A bot's sphere, or the tombstone of a knocked-out one.
#[derive(Component)]
pub struct Bot {
    id: u32,
    dead: bool,
}

#[derive(Resource)]
pub struct BotAssets {
    mesh: Handle<Mesh>,
    alive: Vec<Handle<StandardMaterial>>,
    tombstone: Handle<WorldAsset>,
}

pub fn plugin(app: &mut App) {
    app.init_resource::<Replay>()
        .add_systems(Startup, setup)
        .add_systems(Update, (point_of_view, (replay, sync_bots).chain()));
}

/// Half the arena's side, as in `ARENA_HALF_SIZE` in `sim/src/rules.rs`.
// ponytail: fixed, the server has only ever sent this value; build the
// rooftop from `room.arena_half_size` if arenas start to differ.
const ARENA_HALF_SIZE: f32 = 10.0;

/// A greyboxed rooftop out of KayKit Platformer pieces: a flat 20x20 roof of
/// 4x4 blocks (top at y = 0) with a railing along each edge, standing on the
/// outermost 0.4 m so a bot pushed against the edge touches it.
fn spawn_rooftop(commands: &mut Commands, assets: &AssetServer) {
    let piece = |name: &str| WorldAssetRoot(assets.load(format!("kaykit/{name}.gltf#Scene0")));

    // Blocks are 4x4x4 with their origin at the bottom centre.
    let blocks = (ARENA_HALF_SIZE / 2.0) as i32;
    for x in 0..blocks {
        for z in 0..blocks {
            let at = |i: i32| (i * 4) as f32 + 2.0 - ARENA_HALF_SIZE;
            commands.spawn((
                piece("platform_4x4x4_blue"),
                Transform::from_xyz(at(x), -4.0, at(z)),
            ));
        }
    }

    // Railings fill a 2x2 cell and stand on its -Z side (corners: -Z and
    // -X). Walk the ring of edge cells; a cell is rotated to face outward.
    // Each side starts at its -X corner and stops short of the next one, so
    // every corner is placed once.
    let cells = ARENA_HALF_SIZE as i32; // 2 m cells per side
    let last = ARENA_HALF_SIZE - 1.0; // centre of the outermost cell
    for side in 0..4 {
        let turn = Quat::from_rotation_y(side as f32 * std::f32::consts::FRAC_PI_2);
        for i in 0..cells {
            let name = if i == 0 {
                "railing_corner_single_blue"
            } else {
                "railing_straight_single_blue"
            };
            let at = Vec3::new(i as f32 * 2.0 - last, 0.0, -last);
            commands.spawn((
                piece(name),
                Transform::from_translation(turn * at).with_rotation(turn),
            ));
        }
    }
}

fn setup(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    spawn_rooftop(&mut commands, &assets);

    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::OVERCAST_DAY,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(6.0, 14.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // Moved behind the player's own edge once they are in a room.
    commands.spawn((ArenaCamera, Camera3d::default(), camera_transform(0)));

    commands.insert_resource(BotAssets {
        mesh: meshes.add(Sphere::new(1.0)),
        alive: PALETTE
            .iter()
            .map(|&[r, g, b]| materials.add(Color::srgb_u8(r, g, b)))
            .collect(),
        tombstone: assets.load("kaykit/halloween/gravestone.gltf#Scene0"),
    });
}

/// Everybody watches the arena from behind the edge their own bot starts at.
pub fn point_of_view(screen: Res<Screen>, mut camera: Single<&mut Transform, With<ArenaCamera>>) {
    let Screen::Room(snapshot) = &*screen else {
        return;
    };
    let wanted = camera_transform(snapshot.me.edge);
    if **camera != wanted {
        **camera = wanted;
    }
}

/// Decide which tick to show and blend the frames around it. Without a
/// server (the sandbox) there are no frames, and the bots are filled in
/// right after this.
pub fn replay(
    time: Res<Time>,
    screen: Res<Screen>,
    conn: Option<Res<Conn>>,
    mut replay: ResMut<Replay>,
) {
    replay.bots.clear();
    let Screen::Room(snapshot) = &*screen else {
        return;
    };
    let room = &snapshot.game;

    // We time the replay from the moment we *see* the phase change instead of
    // comparing clocks with the server, so a wrong local clock can't break it.
    let now = time.elapsed_secs_f64();
    let started = match replay.seen {
        Some((id, round, phase, started))
            if id == room.id && round == room.round && phase == room.phase =>
        {
            started
        }
        _ => {
            replay.seen = Some((room.id, room.round, room.phase, now));
            now
        }
    };
    replay.phase_elapsed = (now - started) as f32;

    let last = room.match_ticks as f32;
    let tick = match room.phase {
        Phase::Countdown => 0.0,
        Phase::Fighting => (replay.phase_elapsed * room.ticks_per_sec as f32).min(last),
        // Results, and Prompting for the next round: leave the final frame up.
        Phase::Results | Phase::Prompting => last,
    };

    let Some(conn) = conn else { return };
    let frames = conn.db().frame();
    let Some(from) = frames.key().find(&frame_key(room.id, tick.floor() as u32)) else {
        return;
    };
    let to = frames.key().find(&frame_key(room.id, tick.ceil() as u32));
    let blend = tick.fract();

    for bot in &from.bots {
        // The later frame decides hp/action so hits land when they are shown.
        let next = to
            .as_ref()
            .and_then(|frame| frame.bots.iter().find(|b| b.player_id == bot.player_id))
            .unwrap_or(bot);
        let x = bot.x + (next.x - bot.x) * blend;
        let y = bot.y + (next.y - bot.y) * blend;
        replay.bots.push(BotView {
            id: bot.player_id,
            // Sim (x, y) is the ground plane; Bevy's up axis is Y.
            position: Vec3::new(x, room.bot_radius, y),
            radius: room.bot_radius,
            hp: next.hp,
            action: next.action,
        });
    }
}

/// Keep exactly one sphere per live bot and one tombstone per knocked-out
/// one. A bot that changes between the two is despawned and spawned anew.
pub fn sync_bots(
    mut commands: Commands,
    screen: Res<Screen>,
    replay: Res<Replay>,
    assets: Res<BotAssets>,
    mut bots: Query<(Entity, &Bot, &mut Transform)>,
) {
    // Tombstones face the player's own camera.
    let facing = match &*screen {
        Screen::Room(snapshot) => outward(snapshot.me.edge),
        _ => Vec3::Z,
    };
    let mut present = Vec::new();
    for (entity, bot, mut transform) in &mut bots {
        let view = replay.bots.iter().find(|view| view.id == bot.id);
        match view {
            Some(view) if is_dead(view) == bot.dead => {
                present.push(view.id);
                *transform = bot_transform(view, facing);
            }
            _ => commands.entity(entity).despawn(),
        }
    }

    for view in replay
        .bots
        .iter()
        .filter(|view| !present.contains(&view.id))
    {
        let bot = Bot {
            id: view.id,
            dead: is_dead(view),
        };
        let transform = bot_transform(view, facing);
        if bot.dead {
            commands.spawn((bot, WorldAssetRoot(assets.tombstone.clone()), transform));
        } else {
            let material = assets.alive[view.id as usize % assets.alive.len()].clone();
            commands.spawn((
                bot,
                Mesh3d(assets.mesh.clone()),
                MeshMaterial3d(material),
                transform,
            ));
        }
    }
}

fn is_dead(view: &BotView) -> bool {
    view.action == BotAction::Dead
}

/// Gravestone model: 1.4 m wide, 1.6 m tall, front facing +Z.
const TOMBSTONE_SCALE: f32 = 0.75;

fn bot_transform(view: &BotView, facing: Vec3) -> Transform {
    // Placeholder "animation": a punch puffs the sphere up.
    let scale = match view.action {
        BotAction::Dead => {
            return Transform::from_translation(view.position.with_y(0.0))
                .looking_to(-facing, Vec3::Y)
                .with_scale(Vec3::splat(TOMBSTONE_SCALE));
        }
        BotAction::Attack => 1.3,
        BotAction::Miss => 1.12,
        BotAction::Idle | BotAction::Walk => 1.0,
    };
    Transform::from_translation(view.position).with_scale(Vec3::splat(scale * view.radius))
}
