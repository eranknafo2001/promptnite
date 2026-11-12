//! Effects on top of the replay: when a bot is knocked out it blows up in a
//! cartoon puff, chunks fly off and the camera shakes. The camera also sways
//! gently all the time. Hand-rolled particles, no physics.

use bevy::prelude::*;
use promptnite_bindings::BotAction;

use crate::{
    arena::{self, Replay},
    theme,
};

pub fn plugin(app: &mut App) {
    app.init_resource::<FxState>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (deaths, puffs, debris, camera.after(arena::point_of_view))
                .chain()
                .after(arena::sync_bots),
        );
}

#[derive(Resource)]
struct FxAssets {
    sphere: Handle<Mesh>,
    cube: Handle<Mesh>,
    dead: Handle<StandardMaterial>,
}

#[derive(Resource, Default)]
struct FxState {
    /// Who was dead last frame.
    dead: Vec<(u32, bool)>,
    /// Counts blasts, so each one looks a little different.
    blasts: u32,
    /// Camera shake in 0..=1; it moves by trauma², so it dies down sharply.
    trauma: f32,
    /// The camera's steady transform, and what we last wrote over it.
    steady: Transform,
    written: Transform,
}

#[derive(Component)]
struct Puff {
    age: f32,
    life: f32,
    size: f32,
    velocity: Vec3,
    color: Color,
}

#[derive(Component)]
struct Chunk {
    age: f32,
    size: f32,
    velocity: Vec3,
    spin: Vec3,
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(FxAssets {
        sphere: meshes.add(Sphere::new(1.0)),
        cube: meshes.add(Cuboid::from_length(1.0)),
        dead: materials.add(theme::BOT_DEAD),
    });
}

/// A cheap repeatable random number in 0..1.
pub fn rand(seed: u32) -> f32 {
    let mut x = seed.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

/// A random direction around the up axis, `seed` picking it.
fn around(seed: u32) -> Vec3 {
    let angle = rand(seed) * std::f32::consts::TAU;
    Vec3::new(angle.cos(), 0.0, angle.sin())
}

/// Blow up bots that were alive last frame and are dead now. Bots dead the
/// first time we see them (joining mid-results, Prompting showing the last
/// frame) don't count.
fn deaths(
    mut commands: Commands,
    replay: Res<Replay>,
    assets: Res<FxAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut state: ResMut<FxState>,
) {
    let now: Vec<(u32, bool)> = replay
        .bots
        .iter()
        .map(|bot| (bot.id, bot.action == BotAction::Dead))
        .collect();
    for bot in &replay.bots {
        let was_alive = state.dead.contains(&(bot.id, false));
        if bot.action != BotAction::Dead || !was_alive {
            continue;
        }
        state.blasts += 1;
        state.trauma = (state.trauma + 0.8).min(1.0);
        let seed = state.blasts * 1000;
        let at = bot.position.with_y(bot.radius);
        let [r, g, b] = theme::bot_color(bot.id);
        let own = materials.add(Color::srgb_u8(r, g, b));

        // (colour, how many, size, speed, life): white core in the middle,
        // flames around it, smoke drifting up and lingering.
        let layers = [
            (0, 1, 1.5, 0.0, 0.28),
            (1, 5, 0.85, 2.6, 0.45),
            (2, 6, 0.75, 3.6, 0.6),
            (3, 6, 0.8, 1.6, 0.95),
        ];
        for (i, &(color, count, size, speed, life)) in layers.iter().enumerate() {
            for n in 0..count {
                let s = seed + i as u32 * 50 + n * 3;
                let up = if color == 3 { 1.4 } else { 0.8 };
                let velocity = (around(s) + Vec3::Y * up * rand(s + 1)) * speed;
                let color = theme::FX_PUFF[color];
                commands.spawn((
                    Puff {
                        age: 0.0,
                        life: life * (0.8 + 0.4 * rand(s + 2)),
                        size: size * 1.6 * bot.radius * (0.8 + 0.5 * rand(s + 2)),
                        velocity,
                        color,
                    },
                    Mesh3d(assets.sphere.clone()),
                    MeshMaterial3d(materials.add(StandardMaterial {
                        base_color: color,
                        unlit: true,
                        alpha_mode: AlphaMode::Blend,
                        ..default()
                    })),
                    Transform::from_translation(at + velocity * 0.05).with_scale(Vec3::ZERO),
                ));
            }
        }

        for n in 0..10 {
            let s = seed + 500 + n * 3;
            let material = if n % 3 == 2 {
                assets.dead.clone()
            } else {
                own.clone()
            };
            let size = bot.radius * (0.25 + 0.2 * rand(s + 2));
            commands.spawn((
                Chunk {
                    age: 0.0,
                    size,
                    velocity: around(s) * (2.5 + 3.5 * rand(s + 1))
                        + Vec3::Y * (4.0 + 4.0 * rand(s + 2)),
                    spin: Vec3::new(rand(s) - 0.5, rand(s + 1) - 0.5, rand(s + 2) - 0.5) * 20.0,
                },
                Mesh3d(assets.cube.clone()),
                MeshMaterial3d(material),
                Transform::from_translation(at).with_scale(Vec3::splat(size)),
            ));
        }
    }
    state.dead = now;
}

/// Pop up fast, then shrink and fade out.
fn puffs(
    mut commands: Commands,
    time: Res<Time>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut puffs: Query<(
        Entity,
        &mut Puff,
        &mut Transform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
) {
    let dt = time.delta_secs();
    for (entity, mut puff, mut transform, material) in &mut puffs {
        puff.age += dt;
        let t = puff.age / puff.life;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        // Air drag; smoke keeps rising.
        puff.velocity *= 1.0 - (4.0 * dt).min(1.0);
        transform.translation += puff.velocity * dt;
        let grow = (t / 0.2).min(1.0);
        let scale = 1.0 - (1.0 - grow).powi(3) - 0.6 * ((t - 0.4) / 0.6).max(0.0);
        transform.scale = Vec3::splat(puff.size * scale);
        if let Some(mut material) = materials.get_mut(&material.0) {
            material.base_color = puff.color.with_alpha(1.0 - ((t - 0.5) / 0.5).max(0.0));
        }
    }
}

/// Chunks fly, fall, bounce off the floor and railings, settle, and go.
fn debris(
    mut commands: Commands,
    time: Res<Time>,
    mut chunks: Query<(Entity, &mut Chunk, &mut Transform)>,
) {
    const GRAVITY: f32 = 18.0;
    const LIFE: f32 = 3.5;
    // Inside the railings, as in `arena.rs`.
    const WALL: f32 = 9.5;
    let dt = time.delta_secs();
    for (entity, mut chunk, mut transform) in &mut chunks {
        chunk.age += dt;
        if chunk.age >= LIFE {
            commands.entity(entity).despawn();
            continue;
        }
        chunk.velocity.y -= GRAVITY * dt;
        transform.translation += chunk.velocity * dt;
        let half = chunk.size / 2.0;
        if transform.translation.y < half {
            transform.translation.y = half;
            chunk.velocity.y = -chunk.velocity.y * 0.4;
            chunk.velocity.x *= 0.7;
            chunk.velocity.z *= 0.7;
            chunk.spin *= 0.6;
        }
        for axis in [0, 2] {
            if transform.translation[axis].abs() > WALL {
                transform.translation[axis] = WALL.copysign(transform.translation[axis]);
                chunk.velocity[axis] *= -0.5;
            }
        }
        let spin = chunk.spin * dt;
        transform.rotate(Quat::from_euler(EulerRot::XYZ, spin.x, spin.y, spin.z));
        // Shrink away over the last half second.
        transform.scale = Vec3::splat(chunk.size * ((LIFE - chunk.age) / 0.5).min(1.0));
    }
}

/// Sway the camera gently and jolt it after a blast, on top of its steady
/// transform. In a room `point_of_view` writes the camera every frame;
/// elsewhere nothing does. So if the camera still holds what we wrote last
/// frame, the steady transform is the one we kept; otherwise someone moved
/// it and that is the new steady one. Either way the offsets never build up.
fn camera(
    time: Res<Time>,
    mut state: ResMut<FxState>,
    mut camera: Single<&mut Transform, With<arena::ArenaCamera>>,
) {
    if **camera != state.written {
        state.steady = **camera;
    }
    let mut moved = state.steady;
    let t = time.elapsed_secs();

    // Slow drift: sines at unrelated, long periods so it never visibly loops.
    let drift = |speed: f32, k: f32| (t * speed + k).sin();
    let (right, up) = (moved.right(), moved.up());
    moved.translation += (right * drift(0.31, 0.0) + up * drift(0.23, 2.0)) * theme::CAMERA_SWAY;
    moved.rotate_local_y(drift(0.19, 4.0) * theme::CAMERA_SWAY_TILT);
    moved.rotate_local_x(drift(0.27, 1.0) * theme::CAMERA_SWAY_TILT);

    if state.trauma > 0.0 {
        let amount = state.trauma * state.trauma;
        state.trauma = (state.trauma - time.delta_secs() / theme::FX_SHAKE_SECS).max(0.0);
        // Fast sines at unrelated frequencies make a cheap noise.
        let noise = |k: f32| ((t * 41.0 + k).sin() + (t * 27.0 + k * 3.1).sin()) / 2.0;
        moved.translation +=
            (right * noise(0.0) + up * noise(1.7)) * amount * theme::FX_SHAKE_OFFSET;
        moved.rotate_local_z(noise(4.2) * amount * theme::FX_SHAKE_ROLL);
    }

    **camera = moved;
    state.written = moved;
}
