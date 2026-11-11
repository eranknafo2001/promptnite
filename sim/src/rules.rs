//! Tunable game rules. Change numbers here, not in the server or client.
//!
//! The client never links this crate: the server copies the values it needs
//! for rendering into the public `game` row, so the browser build stays small.

/// Simulation steps per second of match time.
pub const TICKS_PER_SEC: u32 = 10;

/// Hard cap on match length: 60 seconds.
pub const MAX_TICKS: u32 = 60 * TICKS_PER_SEC;

/// The arena is a square centred on the origin, spanning
/// `-ARENA_HALF_SIZE..=ARENA_HALF_SIZE` on both axes.
pub const ARENA_HALF_SIZE: f32 = 10.0;

/// Bots spawn evenly spaced on a circle of this radius.
pub const SPAWN_RADIUS: f32 = 7.0;

/// Collision/visual radius of a bot.
pub const BOT_RADIUS: f32 = 0.5;

pub const MAX_HP: i32 = 100;

/// Distance covered by one `walk` action, in arena units per second.
pub const WALK_SPEED: f32 = 3.0;

/// An `attack` hits the nearest living enemy within this distance.
pub const ATTACK_RANGE: f32 = 1.5;
pub const ATTACK_DAMAGE: i32 = 10;

/// Ticks a bot must wait after attacking before it can attack again.
pub const ATTACK_COOLDOWN_TICKS: u32 = 5;

// --- Sandbox limits for untrusted Lua -------------------------------------

/// VM instructions a script may spend while loading (top-level code).
pub const INIT_FUEL: i32 = 200_000;

/// VM instructions a script may spend per `tick()` call. Going over costs the
/// bot its turn; it does not end the match.
pub const TICK_FUEL: i32 = 20_000;

/// Lua heap size above which a bot is disabled for the rest of the match.
pub const MAX_LUA_MEMORY_BYTES: usize = 8 * 1024 * 1024;

/// Largest script we accept, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 16 * 1024;
