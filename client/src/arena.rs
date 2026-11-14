//! The 3D arena and the match replay.
//!
//! The server simulates a whole match up front and stores one `frame` row per
//! tick. Here we work out which tick should be on screen right now, blend the
//! two neighbouring frames and move one sphere per bot accordingly.

use bevy::{
    mesh::{
        VertexAttributeValues,
        skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
    },
    prelude::*,
};
use promptnite_bindings::{sdk::Table, *};

use crate::{
    net::Conn,
    theme::{self, PALETTE},
    ui::Screen,
};

/// What is on screen this frame. Written here, read by the UI for labels.
#[derive(Resource, Default)]
pub struct Replay {
    pub bots: Vec<BotView>,
    /// Seconds since we saw the current phase begin.
    pub phase_elapsed: f32,
    /// `(room, round, phase)` we last saw, and the app time when it changed.
    seen: Option<(u32, u32, Phase, f64)>,
}

pub struct BotView {
    pub max_hp: i32,
    pub id: u32,
    /// World position of the bot's centre.
    pub position: Vec3,
    pub radius: f32,
    pub hp: i32,
    /// How much of its full stamina the bot has left, `0.0..=1.0`.
    pub stamina: f32,
    pub action: BotAction,
    /// Set for the one tick in which the bot jumps between two teleporters:
    /// the centres of the pad that took it and the pad it comes out of.
    pub teleport: Option<(Vec3, Vec3)>,
}

/// A bot that a teleporter took is on the exact centre of the pad it came out
/// of. So is one that has not moved since, which this tells apart: every jump
/// is longer (`TELEPORTER_SPACING` less `TELEPORTER_RADIUS` in
/// `sim/src/rules.rs`, less the step that took the bot onto the pad).
const TELEPORT_JUMP: f32 = 0.5;

/// The camera that looks at the arena (as opposed to the UI camera).
#[derive(Component)]
pub struct ArenaCamera;

/// A camera pose relative to the player's edge: where the eye is and the
/// point it looks at, each as (outward, up, to the right) from the centre.
struct View {
    eye: Vec3,
    focus: Vec3,
}

/// Watching the fight: tilted, from behind the player's own edge.
const FIGHT_VIEW: View = View {
    eye: Vec3::new(19.0, 21.0, 0.0),
    focus: Vec3::new(1.0, 0.0, 0.0),
};

/// Writing a prompt: lifted high and steeper, though not straight down,
/// for a planning-the-attack feel. Eye and focus both slide to the right
/// (a pan, not a turn) so the arena sits square, left of the prompt column
/// on the right of the screen.
const STRATEGY_VIEW: View = View {
    eye: Vec3::new(11.0, 38.0, 4.5),
    focus: Vec3::new(0.0, 0.0, 4.5),
};

/// Writing a prompt in a narrow window (`ui::COMPACT_WIDTH`): no column to
/// make room for, so no pan, and further away: the arena only has the gap
/// between the players' list and the prompt sheet to show in.
const COMPACT_STRATEGY_VIEW: View = View {
    eye: Vec3::new(14.0, 49.0, 0.0),
    focus: Vec3::ZERO,
};

/// The narrowest window (width over height) both views were composed for.
/// The camera's field of view is vertical, so anything narrower would cut
/// the arena's sides off.
const COMPOSED_ASPECT: f32 = 1.25;

/// `view` from behind edge `edge`, as world (eye, focus), for a window of
/// `size`.
/// The edge table must match `edge_direction` in `sim/src/rules.rs`.
fn place(view: &View, edge: u32, size: Vec2) -> (Vec3, Vec3) {
    let (mut eye, focus) = (view.eye, view.focus);
    // A narrow window (a phone held upright): back off until the arena
    // fits across it again. Wider windows keep the desktop views as composed.
    let aspect = size.x / size.y.max(1.0);
    if size.x < crate::ui::COMPACT_WIDTH && aspect < COMPOSED_ASPECT {
        eye = focus + (eye - focus) * (COMPOSED_ASPECT / aspect);
    }
    let outward = outward(edge);
    let right = Vec3::Y.cross(outward);
    let world = |v: Vec3| outward * v.x + Vec3::Y * v.y + right * v.z;
    (world(eye), world(focus))
}

fn camera_transform(edge: u32, size: Vec2) -> Transform {
    let (eye, focus) = place(&FIGHT_VIEW, edge, size);
    Transform::from_translation(eye).looking_at(focus, Vec3::Y)
}

/// Where the camera's eye and focus are and how fast they move: both
/// follow a spring to the view the game wants.
#[derive(Resource, Default)]
pub struct CameraSpring {
    at: Option<(Vec3, Vec3)>,
    velocity: (Vec3, Vec3),
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
    /// Health last frame, to spot a hit, and the app time of the last one.
    hp: i32,
    hit_at: f32,
}

#[derive(Resource)]
pub struct BotAssets {
    mesh: Handle<Mesh>,
    alive: Vec<Handle<StandardMaterial>>,
    tombstone: Handle<WorldAsset>,
    blade: (Handle<Mesh>, Handle<StandardMaterial>),
    slash: Handle<Mesh>,
    shield: Handle<StandardMaterial>,
    hilt: (Handle<Mesh>, Handle<StandardMaterial>),
    stem: Handle<Mesh>,
    stem_material: Handle<StandardMaterial>,
    stem_bindposes: Handle<SkinnedMeshInverseBindposes>,
    ball: Handle<Mesh>,
}

/// The antenna on a bot's head: a thin stem skinned to a chain of bones and
/// a ball riding the last one. Sizes are in bot radii (the bot is scaled).
const STEM_BASE: f32 = 0.95;
const STEM_LENGTH: f32 = 0.9;
const STEM_RADIUS: f32 = 0.035;
const STEM_BONES: usize = 4;
const BALL_RADIUS: f32 = 0.16;

/// The ball's spring: where it is and how fast it moves, in world space,
/// and the stem's base last frame, to feel the bot speed up and slow down.
#[derive(Component)]
pub struct Antenna {
    tip: Vec3,
    velocity: Vec3,
    base: Vec3,
    base_velocity: Vec3,
    /// The bones that bend, root first.
    bones: Vec<Entity>,
}

/// A live bot's floating hand: a pivot at the bot's centre, turned so the
/// hand (with its dagger pointing outwards) sits off to one side of where
/// the bot faces. Every attack swings it across the front to the other side.
#[derive(Component)]
pub struct Hand {
    id: u32,
    /// Which side it rests on: 1 or -1.
    side: f32,
    /// App time the last swing began.
    swung_at: f32,
    swinging: bool,
    /// Where the bot faces, on the ground plane.
    facing: Vec3,
}

pub fn plugin(app: &mut App) {
    app.init_resource::<Replay>()
        .init_resource::<CameraSpring>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                point_of_view,
                (
                    replay,
                    sync_bots,
                    raise_shields,
                    swing_hands,
                    fade_slashes,
                    lunge,
                    jiggle,
                )
                    .chain(),
            ),
        );
}

/// Half the arena's side, as in `ARENA_HALF_SIZE` in `sim/src/rules.rs`.
// ponytail: fixed, the server has only ever sent this value; build the
// rooftop from `room.arena_half_size` if arenas start to differ.
const ARENA_HALF_SIZE: f32 = 10.0;

/// A greyboxed rooftop out of KayKit Platformer pieces: a flat 20x20 roof of
/// 4x4 blocks (top at y = 0) with a railing along each edge, standing on the
/// outermost 0.4 m (`RAILING_DEPTH` in `sim/src/rules.rs`), where the sim
/// stops the bots.
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

/// The building under the roof: grey walls going down well past the bottom
/// of the screen, with a ledge at every floor, so the arena doesn't float.
fn spawn_building(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    const FLOORS: i32 = 16;
    const FLOOR_HEIGHT: f32 = 3.5;
    // Under the roof blocks, which are 4 m deep.
    const TOP: f32 = -4.0;
    let side = ARENA_HALF_SIZE * 2.0;
    let height = FLOORS as f32 * FLOOR_HEIGHT;
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(side, height, side))),
        MeshMaterial3d(materials.add(theme::BUILDING)),
        Transform::from_xyz(0.0, TOP - height / 2.0, 0.0),
    ));
    let ledge = meshes.add(Cuboid::new(side + 0.4, 0.3, side + 0.4));
    let ledge_material = materials.add(theme::BUILDING_LEDGE);
    for floor in 0..FLOORS {
        commands.spawn((
            Mesh3d(ledge.clone()),
            MeshMaterial3d(ledge_material.clone()),
            Transform::from_xyz(0.0, TOP - floor as f32 * FLOOR_HEIGHT, 0.0),
        ));
    }
}

/// The sky: a backdrop hung far behind everything else, attached to the
/// camera so it always fills the view, fading from light blue at the bottom
/// of the screen to deep blue at the top.
fn sky(meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>) -> impl Bundle {
    // At 500 m a 45° view is about 415 m tall. Just a little taller, so the
    // ends of the fade land at the screen edges with room for the sway, and
    // wide enough for any window shape.
    const DISTANCE: f32 = 500.0;
    let (bottom, top) = (
        theme::BACKGROUND.to_linear().to_f32_array(),
        theme::SKY_TOP.to_linear().to_f32_array(),
    );
    // Rectangle vertices: top right, top left, bottom left, bottom right.
    let mesh = Mesh::from(Rectangle::new(4000.0, 440.0))
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, vec![top, top, bottom, bottom]);
    (
        Mesh3d(meshes.add(mesh)),
        MeshMaterial3d(materials.add(StandardMaterial {
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(0.0, 0.0, -DISTANCE),
    )
}

/// Street level, where our building and every other one stands.
const GROUND: f32 = -60.0;

/// Two rooftop parks on the blocks at opposite corners of ours, so one is in
/// view from every edge. (Down on the street the buildings hide a park.)
fn is_park(x: i32, z: i32) -> bool {
    matches!((x, z), (-2, -2) | (2, 2))
}

/// The city around us: the ground, a grid of other buildings with streets
/// between them, their roofs lower than ours so they never get in the way,
/// some with AC units or a water tank on top, and a couple of parks.
/// Seeded, so it is the same every time.
fn spawn_neighbours(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    use crate::fx::rand;
    // A block per cell; cells next to ours stay empty as the street.
    const CELL: f32 = 12.0;
    // Far enough that the ground at the top of the screen is city too.
    const REACH: i32 = 8;
    let cube = meshes.add(Cuboid::from_length(1.0));
    let tank = meshes.add(Cylinder::new(0.5, 1.0));
    let ball = meshes.add(Sphere::new(0.5));
    let grass = materials.add(theme::PARK_GRASS);
    let leaves = materials.add(theme::TREE_LEAVES);
    let trunk = materials.add(theme::TREE_TRUNK);
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(2000.0, 2000.0))),
        MeshMaterial3d(materials.add(theme::STREET)),
        Transform::from_xyz(0.0, GROUND, 0.0),
    ));
    let walls: Vec<_> = theme::NEIGHBOURS
        .iter()
        .map(|&color| materials.add(color))
        .collect();
    let clutter = materials.add(theme::BUILDING_LEDGE);
    let mut part =
        |mesh: &Handle<Mesh>, material: &Handle<StandardMaterial>, at: Vec3, size: Vec3| {
            commands.spawn((
                Mesh3d(mesh.clone()),
                MeshMaterial3d(material.clone()),
                Transform::from_translation(at).with_scale(size),
            ));
        };

    for x in -REACH..=REACH {
        for z in -REACH..=REACH {
            if x.abs() <= 1 && z.abs() <= 1 {
                continue;
            }
            let seed = ((x + 50) * 131 + (z + 50)) as u32 * 7;
            if is_park(x, z) {
                const ROOF: f32 = -14.0;
                let side = CELL - 1.0;
                let centre = Vec3::new(x as f32 * CELL, ROOF, z as f32 * CELL);
                let height = ROOF - GROUND;
                part(
                    &cube,
                    &walls[0],
                    centre - Vec3::Y * height / 2.0,
                    Vec3::new(side, height, side),
                );
                part(&cube, &grass, centre, Vec3::new(side, 0.4, side));
                for k in 0..7 {
                    let s = seed + 30 + k * 3;
                    let at = centre
                        + Vec3::new(
                            (rand(s) - 0.5) * (side - 4.0),
                            0.0,
                            (rand(s + 1) - 0.5) * (side - 4.0),
                        );
                    let crown = 2.2 + 1.3 * rand(s + 2);
                    part(&tank, &trunk, at + Vec3::Y, Vec3::new(0.4, 2.0, 0.4));
                    part(
                        &ball,
                        &leaves,
                        at + Vec3::Y * (1.6 + crown / 2.0),
                        Vec3::splat(crown),
                    );
                }
                continue;
            }
            // Every so often a gap: a plaza or a wider street.
            if rand(seed) < 0.3 {
                continue;
            }
            let width = CELL * (0.55 + 0.3 * rand(seed + 1));
            let depth = CELL * (0.55 + 0.3 * rand(seed + 2));
            let roof = -14.0 - 30.0 * rand(seed + 3);
            let centre = Vec3::new(
                x as f32 * CELL + (rand(seed + 4) - 0.5) * 2.0,
                roof,
                z as f32 * CELL + (rand(seed + 5) - 0.5) * 2.0,
            );
            let wall = &walls[(rand(seed + 6) * walls.len() as f32) as usize % walls.len()];
            let height = roof - GROUND;
            part(
                &cube,
                wall,
                centre - Vec3::Y * height / 2.0,
                Vec3::new(width, height, depth),
            );
            // Roof clutter, kept away from the edges.
            let spot = |k: u32| {
                centre
                    + Vec3::new(
                        (rand(seed + k) - 0.5) * (width - 3.0),
                        0.0,
                        (rand(seed + k + 1) - 0.5) * (depth - 3.0),
                    )
            };
            for k in 0..(rand(seed + 7) * 3.0) as u32 {
                let size = Vec3::new(1.2, 0.8, 1.6);
                part(
                    &cube,
                    &clutter,
                    spot(10 + k * 2) + Vec3::Y * size.y / 2.0,
                    size,
                );
            }
            if rand(seed + 8) < 0.35 {
                let size = Vec3::new(2.2, 2.4, 2.2);
                part(&tank, &clutter, spot(20) + Vec3::Y * size.y / 2.0, size);
            }
        }
    }
}

fn setup(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
) {
    spawn_rooftop(&mut commands, &assets);
    spawn_building(&mut commands, &mut meshes, &mut materials);
    spawn_neighbours(&mut commands, &mut meshes, &mut materials);

    // A sunny afternoon: a warm sun low enough for long, crisp shadows, and blue sky light
    // filling them in.
    commands.spawn((
        DirectionalLight {
            color: theme::SUN,
            illuminance: theme::SUN_LUX,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(-8.0, 11.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: theme::SKY_LIGHT,
        brightness: theme::SKY_LIGHT_BRIGHTNESS,
        ..default()
    });

    // Moved behind the player's own edge once they are in a room.
    commands.spawn((
        ArenaCamera,
        Camera3d::default(),
        // Any landscape size: `point_of_view` takes over from the first frame.
        camera_transform(0, Vec2::new(1280.0, 720.0)),
        theme::color_grading(),
        children![sky(&mut meshes, &mut materials)],
    ));

    commands.insert_resource(BotAssets {
        mesh: meshes.add(Sphere::new(1.0)),
        alive: PALETTE
            .iter()
            .map(|&[r, g, b]| materials.add(Color::srgb_u8(r, g, b)))
            .collect(),
        tombstone: assets.load("kaykit/halloween/gravestone.gltf#Scene0"),
        blade: (
            meshes.add(Cuboid::new(0.4, 0.1, BLADE_LENGTH)),
            materials.add(theme::DAGGER_BLADE),
        ),
        slash: meshes.add(slash_mesh()),
        shield: materials.add(StandardMaterial {
            base_color: theme::SHIELD,
            unlit: true,
            alpha_mode: AlphaMode::Blend,
            ..default()
        }),
        hilt: (
            meshes.add(Cuboid::new(0.7, 0.14, 0.14)),
            materials.add(theme::DAGGER_HILT),
        ),
        stem: meshes.add(stem_mesh()),
        stem_material: materials.add(Color::srgb(0.55, 0.56, 0.6)),
        // Bone k's rest pose sits k segments up the stem.
        stem_bindposes: bindposes.add(
            (0..=STEM_BONES)
                .map(|k| Mat4::from_translation(-Vec3::Y * (STEM_BASE + k as f32 * SEGMENT)))
                .collect::<Vec<_>>(),
        ),
        ball: meshes.add(Sphere::new(BALL_RADIUS)),
    });
}

const SEGMENT: f32 = STEM_LENGTH / STEM_BONES as f32;

/// A cylinder standing on top of a unit sphere, one ring per bone, each ring
/// skinned wholly to the bone that starts there.
fn stem_mesh() -> Mesh {
    let mut mesh = Cylinder::new(STEM_RADIUS, STEM_LENGTH)
        .mesh()
        .resolution(8)
        .segments(STEM_BONES as u32)
        .build()
        .translated_by(Vec3::Y * (STEM_BASE + STEM_LENGTH / 2.0));
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        unreachable!("primitive meshes have float3 positions");
    };
    let joints: Vec<[u16; 4]> = positions
        .iter()
        .map(|p| [((p[1] - STEM_BASE) / SEGMENT).round() as u16, 0, 0, 0])
        .collect();
    let weights = vec![[1.0, 0.0, 0.0, 0.0]; joints.len()];
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_JOINT_INDEX,
        VertexAttributeValues::Uint16x4(joints),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, weights);
    mesh
}

/// Give the bot `bot` (at `transform`, coloured `material`) its antenna.
fn spawn_antenna(
    commands: &mut Commands,
    assets: &BotAssets,
    bot: Entity,
    material: Handle<StandardMaterial>,
    transform: Transform,
) {
    let mut parent = bot;
    let mut joints = Vec::new();
    for k in 0..=STEM_BONES {
        let y = if k == 0 { STEM_BASE } else { SEGMENT };
        parent = commands
            .spawn((
                Transform::from_xyz(0.0, y, 0.0),
                Visibility::default(),
                ChildOf(parent),
            ))
            .id();
        joints.push(parent);
    }
    commands.spawn((
        Mesh3d(assets.ball.clone()),
        MeshMaterial3d(material),
        Transform::from_xyz(0.0, BALL_RADIUS * 0.6, 0.0),
        ChildOf(parent),
    ));
    commands.spawn((
        Mesh3d(assets.stem.clone()),
        MeshMaterial3d(assets.stem_material.clone()),
        SkinnedMesh {
            inverse_bindposes: assets.stem_bindposes.clone(),
            joints: joints.clone(),
        },
        ChildOf(bot),
    ));
    joints.pop(); // the tip bone only carries the ball
    commands.entity(bot).insert(Antenna {
        tip: transform.transform_point(Vec3::Y * (STEM_BASE + STEM_LENGTH)),
        velocity: Vec3::ZERO,
        base: transform.transform_point(Vec3::Y * STEM_BASE),
        base_velocity: Vec3::ZERO,
        bones: joints,
    });
}

/// A stiff, barely damped spring, like a doorstop's: it rings fast and long
/// rather than swaying. Being stiff it would hardly lean, so every change in
/// the bot's speed kicks the ball harder than real inertia would.
fn jiggle(
    time: Res<Time>,
    mut bots: Query<(&Transform, &mut Antenna)>,
    mut bones: Query<&mut Transform, Without<Antenna>>,
) {
    const STIFFNESS: f32 = 700.0; // ~4 Hz
    const DAMPING: f32 = 4.0; // ~0.08 of critical
    const KICK: f32 = 3.0;
    const MAX_BEND: f32 = 1.2; // radians
    // A stiff spring needs small steps to stay stable.
    const STEPS: u32 = 4;
    let frame = time.delta_secs().min(1.0 / 30.0);
    if frame <= 0.0 {
        return;
    }
    let dt = frame / STEPS as f32;
    for (bot, mut antenna) in &mut bots {
        let base = bot.transform_point(Vec3::Y * STEM_BASE);
        let base_velocity = (base - antenna.base) / frame;
        // Real inertia is the tip staying put while the base moves; this adds
        // the extra.
        let kick = (KICK - 1.0) * (base_velocity - antenna.base_velocity);
        antenna.velocity -= kick;
        antenna.base = base;
        antenna.base_velocity = base_velocity;

        let length = STEM_LENGTH * bot.scale.y;
        let rest = base + bot.rotation * Vec3::Y * length;
        for _ in 0..STEPS {
            let pull = STIFFNESS * (rest - antenna.tip) - DAMPING * antenna.velocity;
            antenna.velocity += pull * dt;
            let tip = antenna.tip + antenna.velocity * dt;
            // The stem doesn't stretch: the ball swings around its base.
            antenna.tip = base + (tip - base).normalize_or(Vec3::Y) * length;
        }
        let lean = (antenna.tip - base).normalize_or(Vec3::Y);
        let (axis, angle) =
            Quat::from_rotation_arc(Vec3::Y, bot.rotation.inverse() * lean).to_axis_angle();
        let per_bone = Quat::from_axis_angle(axis, angle.min(MAX_BEND) / STEM_BONES as f32);
        for &bone in &antenna.bones {
            if let Ok(mut bone) = bones.get_mut(bone) {
                bone.rotation = per_bone;
            }
        }
    }
}

/// Everybody watches the arena from behind the edge their own bot starts at:
/// lifted into the strategy view while writing a prompt, the fight view the
/// rest of the time. The camera springs between the two, a quick
/// snap that overshoots a touch and settles.
pub fn point_of_view(
    time: Res<Time>,
    screen: Res<Screen>,
    mut spring: ResMut<CameraSpring>,
    window: Single<&Window>,
    mut camera: Single<&mut Transform, With<ArenaCamera>>,
) {
    // Stiffness, and damping just under critical for the overshoot.
    const STIFFNESS: f32 = 70.0;
    const DAMPING: f32 = 0.7 * 2.0 * 8.366; // 0.7 × 2√STIFFNESS

    let size = window.size();
    let Screen::Room(snapshot) = &*screen else {
        // Before the first room the camera only has to fit the window.
        if spring.at.is_none() {
            **camera = camera_transform(0, size);
        }
        return;
    };
    // Only while the prompt box is open, as in `ui::draw`: nothing sent
    // yet this round, or the last try failed.
    let writing = snapshot.game.phase == Phase::Prompting
        && matches!(
            snapshot.submission_of(snapshot.me.id).map(|s| s.status),
            None | Some(SubmissionStatus::Failed)
        );
    let view = match (writing, size.x < crate::ui::COMPACT_WIDTH) {
        (true, true) => &COMPACT_STRATEGY_VIEW,
        (true, false) => &STRATEGY_VIEW,
        (false, _) => &FIGHT_VIEW,
    };
    let (eye, focus) = place(view, snapshot.me.edge, size);
    let (mut at, mut focus_at) = spring.at.unwrap_or((eye, focus));
    // A long frame (a hitch, a hidden tab) must not fling the camera.
    let dt = time.delta_secs().min(1.0 / 30.0);
    let (mut v_eye, mut v_focus) = spring.velocity;
    for (pos, vel, goal) in [
        (&mut at, &mut v_eye, eye),
        (&mut focus_at, &mut v_focus, focus),
    ] {
        *vel += (STIFFNESS * (goal - *pos) - DAMPING * *vel) * dt;
        *pos += *vel * dt;
    }
    spring.at = Some((at, focus_at));
    spring.velocity = (v_eye, v_focus);
    **camera = Transform::from_translation(at).looking_at(focus_at, Vec3::Y);
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
    // The server hands the frames out when the fight starts; until they are
    // here there is nothing to draw past the opening one.
    let (floor, ceil) = (tick.floor() as u32, tick.ceil() as u32);
    let (mut from, mut to) = (None, None);
    for frame in conn.db().match_frames().iter() {
        if frame.room_id != room.id {
            continue; // left over from a room we just left
        }
        if frame.tick == ceil {
            to = Some(frame.clone());
        }
        if frame.tick == floor {
            from = Some(frame);
        }
    }
    let Some(from) = from else {
        return;
    };
    let blend = tick.fract();
    // Everybody starts full, so the opening frame says how much that is.
    let opening = conn
        .db()
        .match_frames()
        .iter()
        .find(|f| f.room_id == room.id && f.tick == 0);
    // Stamina sits beside the bots, in the same order.
    let stamina_of = |frame: &Frame, id: u32| {
        let at = frame.bots.iter().position(|b| b.player_id == id)?;
        frame.stamina.as_ref()?.get(at).copied()
    };

    for bot in &from.bots {
        // The later frame decides hp/action so hits land when they are shown.
        let next = to
            .as_ref()
            .and_then(|frame| frame.bots.iter().find(|b| b.player_id == bot.player_id))
            .unwrap_or(bot);
        // Pads are a ring: the one a bot comes out of tells which it went in.
        let pads = &snapshot.teleporters;
        let teleport = pads
            .iter()
            .position(|pad| (pad.x, pad.y) == (next.x, next.y))
            .filter(|_| (next.x - bot.x).hypot(next.y - bot.y) > TELEPORT_JUMP)
            .map(|exit| {
                let entry = &pads[(exit + pads.len() - 1) % pads.len()];
                (
                    Vec3::new(entry.x, 0.0, entry.y),
                    Vec3::new(next.x, 0.0, next.y),
                )
            });
        let jumped = teleport.is_some();
        let (x, y) = match (jumped, blend < 0.5) {
            // A jump is not a slide: here for half the tick, then there.
            (true, true) => (bot.x, bot.y),
            (true, false) => (next.x, next.y),
            (false, _) => (
                bot.x + (next.x - bot.x) * blend,
                bot.y + (next.y - bot.y) * blend,
            ),
        };
        let id = bot.player_id;
        // Blended like the position, so the bar drains and fills smoothly.
        let stamina = stamina_of(&from, id).zip(opening.as_ref().and_then(|f| stamina_of(f, id)));
        let stamina = stamina.map_or(1.0, |(now, full)| {
            let later = to.as_ref().and_then(|f| stamina_of(f, id)).unwrap_or(now);
            let now = now as f32 + (later - now) as f32 * blend;
            (now / full.max(1) as f32).clamp(0.0, 1.0)
        });
        replay.bots.push(BotView {
            max_hp: opening
                .as_ref()
                .and_then(|f| f.bots.iter().find(|b| b.player_id == id))
                .map_or(room.max_hp, |b| b.hp),
            id: bot.player_id,
            // Sim (x, y) is the ground plane; Bevy's up axis is Y.
            position: Vec3::new(x, room.bot_radius, y),
            radius: room.bot_radius,
            hp: next.hp,
            stamina,
            action: next.action,
            teleport,
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
    mut materials: ResMut<Assets<StandardMaterial>>,
    time: Res<Time>,
    mut bots: Query<(
        Entity,
        &mut Bot,
        &mut Transform,
        Option<&MeshMaterial3d<StandardMaterial>>,
    )>,
) {
    let now = time.elapsed_secs();
    // Tombstones face the player's own camera.
    let facing = match &*screen {
        Screen::Room(snapshot) => outward(snapshot.me.edge),
        _ => Vec3::Z,
    };
    let mut present = Vec::new();
    for (entity, mut bot, mut transform, material) in &mut bots {
        let view = replay.bots.iter().find(|view| view.id == bot.id);
        match view {
            Some(view) if is_dead(view) == bot.dead => {
                present.push(view.id);
                if view.hp < bot.hp {
                    bot.hit_at = now;
                }
                bot.hp = view.hp;
                *transform = bot_transform(view, facing);
                // A hit: an orange flash and a quick punch outwards.
                let since = now - bot.hit_at;
                let punch = (1.0 - since / HIT_PUNCH_SECS).max(0.0);
                transform.scale *= 1.0 + HIT_PUNCH * punch * punch;
                // Each hit restarts the fade from full orange. A little past
                // the end too, so the last write is the bot's own colour.
                if since < HIT_FLASH_SECS + 0.5
                    && let Some(mut material) = material.and_then(|m| materials.get_mut(&m.0))
                {
                    let fade = (1.0 - since / HIT_FLASH_SECS).clamp(0.0, 1.0);
                    let flash = fade * fade;
                    let [r, g, b] = theme::bot_color(bot.id);
                    material.base_color = Color::srgb_u8(r, g, b).mix(&theme::HIT_FLASH, flash);
                    material.emissive = theme::HIT_FLASH.to_linear() * (2.0 * flash);
                }
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
            hp: view.hp,
            hit_at: f32::NEG_INFINITY,
        };
        let transform = bot_transform(view, facing);
        if bot.dead {
            commands.spawn((bot, WorldAssetRoot(assets.tombstone.clone()), transform));
        } else {
            let material = assets.alive[view.id as usize % assets.alive.len()].clone();
            let id = view.id;
            // Its own copy of the body's material, for the hit flash.
            let body = materials.get(&material).cloned().unwrap_or_default();
            let body = materials.add(body);
            let bot = commands
                .spawn((
                    bot,
                    Mesh3d(assets.mesh.clone()),
                    MeshMaterial3d(body),
                    transform,
                    children![
                        (
                            Bubble(id),
                            Mesh3d(assets.mesh.clone()),
                            MeshMaterial3d(assets.shield.clone()),
                            Transform::from_scale(Vec3::splat(BUBBLE_SIZE)),
                            Visibility::Hidden,
                            bevy::light::NotShadowCaster,
                        ),
                        (
                            Nose(id),
                            Transform::default(),
                            Visibility::default(),
                            children![(
                                Mesh3d(assets.mesh.clone()),
                                MeshMaterial3d(material.clone()),
                                Transform::from_xyz(0.0, NOSE_LIFT, -NOSE_REACH)
                                    .with_scale(Vec3::splat(NOSE_SIZE)),
                            )],
                        ),
                        (
                            Hand {
                                id,
                                side: 1.0,
                                swung_at: f32::NEG_INFINITY,
                                swinging: false,
                                facing: Vec3::NEG_Z,
                            },
                            Transform::default(),
                            Visibility::default(),
                            children![(
                                Mesh3d(assets.mesh.clone()),
                                MeshMaterial3d(material.clone()),
                                Transform::from_xyz(0.0, 0.0, -HAND_REACH)
                                    .with_scale(Vec3::splat(HAND_SIZE)),
                                children![
                                    // Hand radii along z; the meshes themselves in bot radii.
                                    (
                                        Mesh3d(assets.blade.0.clone()),
                                        MeshMaterial3d(assets.blade.1.clone()),
                                        Transform::from_xyz(
                                            0.0,
                                            0.0,
                                            -0.6 - BLADE_LENGTH / HAND_SIZE / 2.0
                                        )
                                        .with_scale(Vec3::splat(1.0 / HAND_SIZE)),
                                    ),
                                    (
                                        Mesh3d(assets.hilt.0.clone()),
                                        MeshMaterial3d(assets.hilt.1.clone()),
                                        Transform::from_xyz(0.0, 0.0, -1.0)
                                            .with_scale(Vec3::splat(1.0 / HAND_SIZE)),
                                    ),
                                ],
                            )],
                        )
                    ],
                ))
                .id();
            spawn_antenna(&mut commands, &assets, bot, material, transform);
        }
    }
}

/// The bubble round a live bot, shown on the ticks its shield is up.
#[derive(Component)]
pub struct Bubble(u32);

/// In bot radii: clear of the body, inside the hand.
const BUBBLE_SIZE: f32 = 1.4;

fn raise_shields(replay: Res<Replay>, mut bubbles: Query<(&Bubble, &mut Visibility)>) {
    for (bubble, mut visibility) in &mut bubbles {
        let up = replay
            .bots
            .iter()
            .any(|bot| bot.id == bubble.0 && bot.action == BotAction::Shield);
        visibility.set_if_neq(if up {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
    }
}

fn is_dead(view: &BotView) -> bool {
    view.action == BotAction::Dead
}

/// Gravestone model: 1.4 m wide, 1.6 m tall, front facing +Z.
const TOMBSTONE_SCALE: f32 = 0.75;

fn bot_transform(view: &BotView, facing: Vec3) -> Transform {
    if is_dead(view) {
        return Transform::from_translation(view.position.with_y(0.0))
            .looking_to(-facing, Vec3::Y)
            .with_scale(Vec3::splat(TOMBSTONE_SCALE));
    }
    Transform::from_translation(view.position).with_scale(Vec3::splat(view.radius))
}

/// Turned to where its bot faces, with a little sphere sticking out of the
/// body that way.
#[derive(Component)]
pub struct Nose(u32);

/// The nose, in bot radii: how far out from the centre, how high, how big.
const NOSE_REACH: f32 = 0.95;
const NOSE_LIFT: f32 = 0.2;
const NOSE_SIZE: f32 = 0.28;

/// A hit: how long the body's orange takes to fade, and how far and how long it punches out.
const HIT_FLASH_SECS: f32 = 0.25;
const HIT_PUNCH: f32 = 0.12;
const HIT_PUNCH_SECS: f32 = 0.2;

/// The hand, in bot radii: how far out it floats and how big it is.
const HAND_REACH: f32 = 1.6;
const HAND_SIZE: f32 = 0.3;
/// In bot radii, like the hilt.
const BLADE_LENGTH: f32 = 0.9;
/// How far round from straight ahead the hand rests, either side.
const HAND_REST: f32 = 1.1;
const SWING_SECS: f32 = 0.18;
/// Each swing jolts the body forwards and back, in bot radii and seconds.
/// Only on screen: the replayed position is left alone.
const LUNGE: f32 = 0.3;
const LUNGE_SECS: f32 = 0.25;

/// Noses and trails, which turn with the hands.
type Turning = (
    AnyOf<(&'static Nose, &'static Slash)>,
    &'static mut Transform,
);

/// Jolt each swinging bot's body towards where it faces: out fast, peaking a
/// quarter of the way in, then eased back.
fn lunge(
    time: Res<Time>,
    hands: Query<(&Hand, &ChildOf)>,
    mut bodies: Query<&mut Transform, With<Bot>>,
) {
    let now = time.elapsed_secs();
    for (hand, body) in &hands {
        let t = ((now - hand.swung_at) / LUNGE_SECS).clamp(0.0, 1.0);
        let lunge = 9.48 * t * (1.0 - t).powi(3);
        if let Ok(mut body) = bodies.get_mut(body.parent()) {
            // The body is scaled to its radius.
            let reach = LUNGE * body.scale.x * lunge;
            body.translation += hand.facing * reach;
        }
    }
}

/// Face the nearest living enemy; on every swing (an attack tick, hit or
/// miss) sweep the hand across the front to the other side.
fn swing_hands(
    mut commands: Commands,
    assets: Res<BotAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    time: Res<Time>,
    replay: Res<Replay>,
    mut hands: Query<(&mut Hand, &mut Transform)>,
    mut turning: Query<Turning, Without<Hand>>,
) {
    let now = time.elapsed_secs();
    for (mut hand, mut transform) in &mut hands {
        let Some(me) = replay.bots.iter().find(|bot| bot.id == hand.id) else {
            continue;
        };
        // ponytail: frames carry no heading, so bots face their nearest
        // enemy; send one from the sim if bots ever need to look elsewhere.
        let nearest = replay
            .bots
            .iter()
            .filter(|bot| bot.id != me.id && !is_dead(bot))
            .map(|bot| (bot.position - me.position).with_y(0.0))
            .filter(|to| to.length_squared() > 1e-4)
            .min_by(|a, b| a.length_squared().total_cmp(&b.length_squared()));
        if let Some(to) = nearest {
            let goal = to.normalize();
            let turn = 1.0 - (-12.0 * time.delta_secs()).exp();
            hand.facing = hand.facing.lerp(goal, turn).normalize_or(goal);
        }

        let swinging = matches!(me.action, BotAction::Attack | BotAction::Miss);
        if swinging && !hand.swinging {
            hand.side = -hand.side;
            hand.swung_at = now;
            // The trail behind the blade, mirrored to run the way it goes.
            // Placed by the code below from the next frame on.
            let scale = Vec3::new(hand.side, 1.0, 1.0) * me.radius;
            commands.spawn((
                Slash {
                    id: hand.id,
                    born: now,
                    scale,
                },
                Mesh3d(assets.slash.clone()),
                MeshMaterial3d(materials.add(StandardMaterial {
                    // Past white, so the glow holds up over a bright floor.
                    base_color: (theme::SLASH.to_linear() * 2.5).into(),
                    unlit: true,
                    // Added on top, so it glows against the floor.
                    alpha_mode: AlphaMode::Add,
                    double_sided: true,
                    cull_mode: None,
                    ..default()
                })),
                Transform::from_scale(Vec3::ZERO),
                bevy::light::NotShadowCaster,
            ));
        }
        hand.swinging = swinging;

        // Ease out: fast through the front, settling on the far side.
        let t = ((now - hand.swung_at) / SWING_SECS).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        let offset = HAND_REST * hand.side * (2.0 * eased - 1.0);
        let yaw = f32::atan2(-hand.facing.x, -hand.facing.z) + offset;
        transform.rotation = Quat::from_rotation_y(yaw);
        let ahead = Quat::from_rotation_y(yaw - offset);
        for ((nose, slash), mut turned) in &mut turning {
            if nose.is_some_and(|nose| nose.0 == hand.id) {
                turned.rotation = ahead;
            }
            // This swing's trail rides along with the blade: its bright end
            // (at +HAND_REST in the mesh, mirrored for the other way) on it.
            if let Some(slash) = slash
                && slash.id == hand.id
                && slash.born == hand.swung_at
                && t < 1.0
            {
                turned.translation = me.position;
                turned.rotation = Quat::from_rotation_y(yaw - hand.side * HAND_REST);
                // Grows out of the blade as the swing gets going.
                turned.scale = slash.scale * (0.6 + 0.4 * eased);
            }
        }
    }
}

/// A swing's trail: shown at once, gone after `SLASH_SECS`.
#[derive(Component)]
struct Slash {
    /// The bot, and the swing (its start time) it belongs to.
    id: u32,
    born: f32,
    scale: Vec3,
}

/// How long a trail lingers once its swing is over.
const SLASH_SECS: f32 = 0.2;

/// Fade each trail out once its swing is over, spreading it a little as it
/// goes.
fn fade_slashes(
    mut commands: Commands,
    time: Res<Time>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut slashes: Query<(
        Entity,
        &Slash,
        &MeshMaterial3d<StandardMaterial>,
        &mut Transform,
    )>,
) {
    let now = time.elapsed_secs();
    for (entity, slash, material, mut transform) in &mut slashes {
        // Full strength while its swing lasts, then fade.
        let t = (now - slash.born - SWING_SECS) / SLASH_SECS;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        if t < 0.0 {
            continue;
        }
        if let Some(mut material) = materials.get_mut(&material.0) {
            material.base_color.set_alpha(1.0 - t * t);
        }
        transform.scale = slash.scale * (1.0 + 0.12 * t);
    }
}

/// Torn streaks round the bot in its own radii, sweeping from one resting
/// side of the hand to the other (towards +x), where the blade passes. Each
/// streak is a thin band (soft edge, hot core, soft edge) that starts
/// somewhere along the swing, thickens and brightens towards its end, and
/// is ripped through by gaps and a ragged width. All of it in vertex alpha.
fn slash_mesh() -> Mesh {
    use bevy::{
        asset::RenderAssetUsages, mesh::Indices, render::render_resource::PrimitiveTopology,
    };

    const SEGMENTS: u32 = 96;
    // (radius, half width, where along the swing it starts, brightness)
    const STREAKS: [(f32, f32, f32, f32); 6] = [
        (2.15, 0.3, 0.0, 1.0),
        (1.8, 0.16, 0.3, 0.9),
        (2.5, 0.11, 0.5, 0.8),
        (1.55, 0.1, 0.55, 0.6),
        (1.95, 0.07, 0.1, 0.7),
        (2.35, 0.07, 0.2, 0.7),
    ];
    let (mut positions, mut colors, mut indices) = (Vec::new(), Vec::new(), Vec::new());
    for (k, &(radius, half_width, start, brightness)) in STREAKS.iter().enumerate() {
        let seed = k as f32 * 7.31;
        let first = positions.len() as u32;
        for i in 0..=SEGMENTS {
            let u = i as f32 / SEGMENTS as f32;
            let angle = HAND_REST * (2.0 * u - 1.0);
            // The same turn the hand pivot makes: -z, round y.
            let dir = Vec3::new(-angle.sin(), 0.0, -angle.cos());
            // 0 before the streak starts, rising to 1 at the end of the swing.
            let along = ((u - start) / (1.0 - start)).clamp(0.0, 1.0);
            // ponytail: a few sines stand in for noise; a texture if it ever
            // needs to look less regular.
            let wobble = (u * 61.0 + seed).sin() * 0.5 + (u * 173.0 + seed * 2.3).sin() * 0.5;
            // Long strokes with the odd break; the main one barely breaks.
            let breaks = if k == 0 { 2.6 } else { 1.4 };
            let gaps =
                ((u * 14.0 + seed * 1.7).sin() + (u * 33.0 + seed).sin() * 0.6) * 2.0 + breaks;
            let width = half_width * (0.25 + 0.75 * along) * (1.0 + 0.6 * wobble);
            let alpha = brightness * along.sqrt() * gaps.clamp(0.0, 1.0);
            for (offset, edge) in [(-width, 0.0), (0.0, 1.0), (width, 0.0)] {
                positions.push(dir * (radius + offset));
                colors.push([1.0, 1.0, 1.0, alpha * edge]);
            }
        }
        for i in 0..SEGMENTS {
            for j in 0..2 {
                let a = first + i * 3 + j;
                let b = a + 3;
                indices.extend([a, b, a + 1, a + 1, b, b + 1]);
            }
        }
    }
    let count = positions.len();
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 1.0, 0.0]; count])
    .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
    .with_inserted_indices(Indices::U32(indices))
}
