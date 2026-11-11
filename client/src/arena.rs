//! The 3D arena and the match replay.
//!
//! The server simulates a whole match up front and stores one `frame` row per
//! tick. Here we work out which tick should be on screen right now, blend the
//! two neighbouring frames and move one sphere per bot accordingly.

use bevy::prelude::*;
use promptnite_bindings::*;

use crate::net::{self, Conn};

/// Colours are assigned by bot id, so a player keeps theirs between rounds.
/// Shared with the UI so the player list matches the spheres.
pub const PALETTE: [[u8; 3]; 8] = [
    [239, 83, 80],
    [66, 165, 245],
    [102, 187, 106],
    [255, 202, 40],
    [171, 71, 188],
    [255, 112, 67],
    [38, 198, 218],
    [236, 64, 122],
];

pub fn bot_color(id: u32) -> [u8; 3] {
    PALETTE[id as usize % PALETTE.len()]
}

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

#[derive(Component)]
struct Floor;

#[derive(Component)]
struct Bot(u32);

#[derive(Resource)]
struct BotAssets {
    mesh: Handle<Mesh>,
    alive: Vec<Handle<StandardMaterial>>,
    dead: Handle<StandardMaterial>,
}

pub fn plugin(app: &mut App) {
    app.init_resource::<Replay>()
        .add_systems(Startup, setup)
        .add_systems(Update, (replay, sync_bots).chain());
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // A unit square, scaled to the arena size once the server tells us.
    commands.spawn((
        Floor,
        Mesh3d(meshes.add(Plane3d::default().mesh().size(1.0, 1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.55, 0.56, 0.58),
            perceptual_roughness: 0.9,
            ..default()
        })),
        Transform::from_scale(Vec3::new(20.0, 1.0, 20.0)),
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::OVERCAST_DAY,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(6.0, 14.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // Tilted view from one side, like the original sketch.
    commands.spawn((
        ArenaCamera,
        Camera3d::default(),
        Transform::from_xyz(0.0, 21.0, 19.0).looking_at(Vec3::new(0.0, 0.0, 1.0), Vec3::Y),
    ));

    commands.insert_resource(BotAssets {
        mesh: meshes.add(Sphere::new(1.0)),
        alive: PALETTE
            .iter()
            .map(|&[r, g, b]| materials.add(Color::srgb_u8(r, g, b)))
            .collect(),
        dead: materials.add(Color::srgb(0.25, 0.25, 0.27)),
    });
}

/// Decide which tick to show and blend the frames around it.
fn replay(
    time: Res<Time>,
    conn: Option<Res<Conn>>,
    mut replay: ResMut<Replay>,
    mut floor: Single<&mut Transform, With<Floor>>,
) {
    replay.bots.clear();
    let Some(conn) = conn else { return };
    let Some((_, room)) = net::my_room(&conn) else {
        return;
    };
    floor.scale = Vec3::new(room.arena_half_size * 2.0, 1.0, room.arena_half_size * 2.0);

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

/// Keep exactly one sphere per bot in the replay.
fn sync_bots(
    mut commands: Commands,
    replay: Res<Replay>,
    assets: Res<BotAssets>,
    mut spheres: Query<(
        Entity,
        &Bot,
        &mut Transform,
        &mut MeshMaterial3d<StandardMaterial>,
    )>,
) {
    let mut present = Vec::new();
    for (entity, bot, mut transform, mut material) in &mut spheres {
        let Some(view) = replay.bots.iter().find(|view| view.id == bot.0) else {
            commands.entity(entity).despawn();
            continue;
        };
        present.push(view.id);
        *transform = bot_transform(view);
        let wanted = bot_material(&assets, view);
        if material.0 != wanted {
            material.0 = wanted;
        }
    }

    for view in replay
        .bots
        .iter()
        .filter(|view| !present.contains(&view.id))
    {
        commands.spawn((
            Bot(view.id),
            Mesh3d(assets.mesh.clone()),
            MeshMaterial3d(bot_material(&assets, view)),
            bot_transform(view),
        ));
    }
}

fn bot_transform(view: &BotView) -> Transform {
    // Placeholder "animation": a punch puffs the sphere up, a knocked-out bot
    // is squashed onto the floor.
    let (scale, height) = match view.action {
        BotAction::Attack => (Vec3::splat(1.3), 1.0),
        BotAction::Miss => (Vec3::splat(1.12), 1.0),
        BotAction::Dead => (Vec3::new(1.15, 0.3, 1.15), 0.3),
        BotAction::Idle | BotAction::Walk => (Vec3::ONE, 1.0),
    };
    Transform::from_translation(view.position.with_y(view.radius * height))
        .with_scale(scale * view.radius)
}

fn bot_material(assets: &BotAssets, view: &BotView) -> Handle<StandardMaterial> {
    if view.action == BotAction::Dead {
        assets.dead.clone()
    } else {
        assets.alive[view.id as usize % assets.alive.len()].clone()
    }
}
