//! Teleporter pads on the arena floor.
//!
//! The room's owner places them between matches (`ui::teleporters`); a bot
//! that walks onto one can come out of any other. The pad is modelled in
//! Blender (`assets/blender/teleporter.py`): a dark dish, the lit segments
//! set into it, and a flame that this module raises over both pads whenever
//! a bot jumps.

use bevy::{gltf::GltfAssetLabel, light::NotShadowCaster, prelude::*};
use promptnite_bindings::Teleporter;

use crate::{
    arena::{self, Replay},
    theme,
    ui::Screen,
};

// The server's rules for placing a pad (`sim/src/rules.rs`), repeated here so
// the owner sees where one fits before asking for it.
/// `TELEPORTER_RADIUS`.
pub const RADIUS: f32 = 0.9;
/// `TELEPORTER_SPACING`.
const SPACING: f32 = 2.5;
/// `MAX_TELEPORTERS`.
pub const MAX: usize = 8;
/// `BOT_LIMIT`: a bot has to be able to stand on the pad's centre.
const LIMIT: f32 = 9.1;

/// How long the flame stands after a jump, in seconds.
const FLARE_SECS: f32 = 0.7;
/// Share of that time the flame takes to shoot up.
const FLARE_RISE: f32 = 0.12;
/// A jump lights the pads this close to its two ends. A bot is exactly on
/// them, so anything well under `SPACING` does.
const REACH: f32 = 0.5;

/// Why the owner cannot put a pad at `(x, y)`, if they cannot.
pub fn refusal(pads: &[Teleporter], x: f32, y: f32) -> Option<&'static str> {
    if x.abs() > LIMIT || y.abs() > LIMIT {
        Some("Too close to the railing")
    } else if pads.len() >= MAX {
        Some("No teleporters left")
    } else if pads
        .iter()
        .any(|pad| (pad.x - x).hypot(pad.y - y) < SPACING)
    {
        Some("Too close to another teleporter")
    } else {
        None
    }
}

/// The pad's three meshes, in the order the Blender script exports them.
#[derive(Resource)]
struct PadAssets {
    dish: Handle<Mesh>,
    lights: Handle<Mesh>,
    flame: Handle<Mesh>,
    metal: Handle<StandardMaterial>,
}

/// One pad on the floor. Its lights and flame are children with materials of
/// their own, as each pad flares by itself.
#[derive(Component)]
struct Pad {
    number: u32,
    /// Where it stands, in sim coordinates.
    at: Vec2,
    /// App time somebody last jumped from or to it.
    flared_at: f32,
    lights: Handle<StandardMaterial>,
    flame: Handle<StandardMaterial>,
}

#[derive(Component)]
struct Flame;

pub fn plugin(app: &mut App) {
    app.add_systems(Startup, setup)
        .add_systems(Update, (sync_pads, flare).chain().after(arena::sync_bots));
}

fn setup(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mesh = |mesh| {
        assets.load(GltfAssetLabel::Primitive { mesh, primitive: 0 }.from_asset("teleporter.gltf"))
    };
    commands.insert_resource(PadAssets {
        dish: mesh(0),
        lights: mesh(1),
        flame: mesh(2),
        metal: materials.add(StandardMaterial {
            base_color: theme::TELEPORTER_METAL,
            metallic: 0.85,
            perceptual_roughness: 0.42,
            ..default()
        }),
    });
}

/// Keep exactly one pad on the floor per teleporter of the room.
fn sync_pads(
    mut commands: Commands,
    screen: Res<Screen>,
    assets: Res<PadAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    time: Res<Time>,
    pads: Query<(Entity, &Pad)>,
) {
    let wanted: &[Teleporter] = match &*screen {
        Screen::Room(snapshot) => &snapshot.teleporters,
        _ => &[],
    };
    let same =
        |pad: &Pad, row: &Teleporter| pad.number == row.number && pad.at == Vec2::new(row.x, row.y);

    for (entity, pad) in &pads {
        if !wanted.iter().any(|row| same(pad, row)) {
            commands.entity(entity).despawn();
        }
    }
    for row in wanted {
        if pads.iter().any(|(_, pad)| same(pad, row)) {
            continue;
        }
        let lights = materials.add(StandardMaterial {
            base_color: theme::TELEPORTER_GLOW.with_luminance(0.15),
            perceptual_roughness: 0.3,
            ..default()
        });
        let flame = materials.add(StandardMaterial {
            unlit: true,
            // Added on top, so it glows against the floor. How see-through
            // it is along its height comes with the mesh.
            alpha_mode: AlphaMode::Add,
            double_sided: true,
            cull_mode: None,
            ..default()
        });
        commands.spawn((
            Pad {
                number: row.number,
                at: Vec2::new(row.x, row.y),
                // A new pad flares once, so the owner sees it land.
                flared_at: time.elapsed_secs(),
                lights: lights.clone(),
                flame: flame.clone(),
            },
            // Sim (x, y) is the ground plane; Bevy's up axis is Y.
            Transform::from_xyz(row.x, 0.0, row.y),
            Visibility::default(),
            children![
                (
                    Mesh3d(assets.dish.clone()),
                    MeshMaterial3d(assets.metal.clone())
                ),
                (Mesh3d(assets.lights.clone()), MeshMaterial3d(lights)),
                (
                    Flame,
                    Mesh3d(assets.flame.clone()),
                    MeshMaterial3d(flame),
                    Transform::from_scale(Vec3::ZERO),
                    NotShadowCaster,
                ),
            ],
        ));
    }
}

/// The lights breathe; a jump brightens them and raises the flame over both
/// of its pads, which then dies down again.
fn flare(
    time: Res<Time>,
    replay: Res<Replay>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut pads: Query<&mut Pad>,
    mut flames: Query<(&ChildOf, &mut Transform), With<Flame>>,
) {
    let now = time.elapsed_secs();
    let glow = theme::TELEPORTER_GLOW.to_linear();

    for mut pad in &mut pads {
        let jumped = replay
            .bots
            .iter()
            .filter_map(|bot| bot.teleport)
            .flat_map(|(from, to)| [from, to])
            .any(|end| end.xz().distance(pad.at) < REACH);
        // Not while it is still rising, or it would never get off the pad.
        if jumped && now - pad.flared_at > FLARE_SECS * FLARE_RISE {
            pad.flared_at = now;
        }

        let t = ((now - pad.flared_at) / FLARE_SECS).clamp(0.0, 1.0);
        let lit = (1.0 - t).powi(2);
        if let Some(mut lights) = materials.get_mut(&pad.lights) {
            // Each pad breathes on its own beat.
            let breath = 0.5 + 0.5 * (now * 2.2 + pad.number as f32 * 1.7).sin();
            // Nothing blooms here, so anything much past 1 only washes out
            // to white: stay cyan at rest and go white for the jump.
            lights.emissive = glow * (0.8 + 0.25 * breath + 4.0 * lit);
        }
        if let Some(mut flame) = materials.get_mut(&pad.flame) {
            // Past white at its brightest, so it holds up over a bright floor.
            flame.base_color = (glow * 2.5).with_alpha(lit).into();
        }
    }

    for (child_of, mut transform) in &mut flames {
        let Ok(pad) = pads.get(child_of.parent()) else {
            continue;
        };
        let t = ((now - pad.flared_at) / FLARE_SECS).clamp(0.0, 1.0);
        // Shoots up fast, then sinks a little as it fades.
        let rise = (t / FLARE_RISE).min(1.0);
        let height = (1.0 - (1.0 - rise).powi(3)) * (1.0 - 0.25 * t);
        *transform = if t < 1.0 {
            Transform::from_rotation(Quat::from_rotation_y(now * 5.0))
                .with_scale(Vec3::new(1.0, height, 1.0))
        } else {
            Transform::from_scale(Vec3::ZERO)
        };
    }
}
