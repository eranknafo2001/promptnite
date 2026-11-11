//! Sandboxed Lua runner for one bot.
//!
//! Scripts are untrusted (an LLM wrote them from a player's prompt), so each
//! bot gets its own VM with:
//!   * only the pure parts of the stdlib (no `io`, no `os`),
//!   * an instruction budget per call (see [`rules::TICK_FUEL`]),
//!   * a heap cap (see [`rules::MAX_LUA_MEMORY_BYTES`]),
//!   * a seeded `math.random`, so a match replays identically.

use piccolo::{
    Closure, Context, Executor, Fuel, Lua, StashedExecutor, StashedFunction, Table, Value, stdlib,
};

use crate::{rules, sim::Action};

/// What a bot can see when it decides its action.
pub struct BotView<'a> {
    pub tick: u32,
    pub me: Seen,
    pub cooldown: u32,
    /// Living enemies only.
    pub enemies: &'a [Seen],
}

#[derive(Clone, Copy)]
pub struct Seen {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub hp: i32,
    /// Distance from the observing bot (0 for `me`).
    pub dist: f32,
}

pub struct LuaBot {
    lua: Lua,
    executor: StashedExecutor,
    tick_fn: StashedFunction,
}

impl LuaBot {
    /// Compile `source`, run its top-level code and grab the global `tick`.
    pub fn new(source: &str, seed: u64) -> Result<Self, String> {
        if source.len() > rules::MAX_SCRIPT_BYTES {
            return Err(format!(
                "script is {} bytes, the limit is {}",
                source.len(),
                rules::MAX_SCRIPT_BYTES
            ));
        }

        let mut lua = Lua::empty();
        lua.enter(|ctx| {
            stdlib::load_base(ctx);
            stdlib::load_math(ctx);
            stdlib::load_string(ctx);
            stdlib::load_table(ctx);
        });
        let executor = lua.enter(|ctx| ctx.stash(Executor::new(ctx)));

        // Seed first so the player's own top-level code already sees a
        // deterministic RNG. Kept as a separate chunk so error line numbers
        // in the player's script stay correct.
        run_chunk(
            &mut lua,
            &executor,
            "seed",
            &format!("math.randomseed({seed})"),
        )?;
        run_chunk(&mut lua, &executor, "bot", source)?;

        let tick_fn = lua.enter(|ctx| match ctx.get_global("tick") {
            Value::Function(f) => Ok(ctx.stash(f)),
            other => Err(format!(
                "script must define a global function `tick(state)`, found {}",
                other.type_name()
            )),
        })?;

        Ok(Self {
            lua,
            executor,
            tick_fn,
        })
    }

    /// Ask the script for this tick's action.
    ///
    /// An `Err` means the script misbehaved (runtime error, budget exceeded,
    /// nonsense return value). The caller decides what that costs the bot.
    pub fn tick(&mut self, view: &BotView) -> Result<Action, String> {
        let (executor, tick_fn) = (&self.executor, &self.tick_fn);

        let mut fuel = Fuel::with(rules::TICK_FUEL);
        let finished = self.lua.enter(|ctx| {
            let state = build_state(ctx, view);
            let executor = ctx.fetch(executor);
            executor.restart(ctx, ctx.fetch(tick_fn), state);
            executor.step(ctx, &mut fuel)
        });
        if !finished {
            return Err(format!(
                "tick() used more than {} instructions",
                rules::TICK_FUEL
            ));
        }

        self.lua
            .enter(|ctx| match ctx.fetch(executor).take_result::<Value>(ctx) {
                Ok(Ok(value)) => parse_action(ctx, value),
                Ok(Err(err)) => Err(err.to_string()),
                Err(err) => Err(err.to_string()),
            })
    }

    /// Current Lua heap size in bytes.
    pub fn memory(&self) -> usize {
        self.lua.total_memory()
    }
}

/// Check that a script compiles, loads within budget and defines `tick`.
/// Used by the server to reject a bad script before the round starts.
pub fn validate_script(source: &str) -> Result<(), String> {
    LuaBot::new(source, 0).map(|_| ())
}

fn run_chunk(
    lua: &mut Lua,
    executor: &StashedExecutor,
    name: &str,
    source: &str,
) -> Result<(), String> {
    lua.enter(|ctx| {
        let closure =
            Closure::load(ctx, Some(name), source.as_bytes()).map_err(|err| err.to_string())?;
        ctx.fetch(executor).restart(ctx, closure.into(), ());
        Ok::<(), String>(())
    })?;

    let mut fuel = Fuel::with(rules::INIT_FUEL);
    if !lua.enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel)) {
        return Err(format!(
            "top-level code used more than {} instructions",
            rules::INIT_FUEL
        ));
    }

    lua.enter(|ctx| match ctx.fetch(executor).take_result::<Value>(ctx) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(err) => Err(err.to_string()),
    })
}

/// The `state` table handed to `tick`. Keep this in sync with the agent's
/// system prompt in the server (`server/src/agent.rs`).
fn build_state<'gc>(ctx: Context<'gc>, view: &BotView) -> Table<'gc> {
    let state = Table::new(&ctx);
    set(ctx, state, "tick", view.tick as i64);

    let me = seen_table(ctx, &view.me);
    set(ctx, me, "cooldown", view.cooldown as i64);
    set(ctx, state, "me", me);

    let enemies = Table::new(&ctx);
    for (i, enemy) in view.enemies.iter().enumerate() {
        let row = seen_table(ctx, enemy);
        set(ctx, row, "dist", enemy.dist as f64);
        // Lua arrays are 1-based.
        let _ = enemies.set(ctx, (i + 1) as i64, row);
    }
    set(ctx, state, "enemies", enemies);

    let arena = Table::new(&ctx);
    set(ctx, arena, "half_size", rules::ARENA_HALF_SIZE as f64);
    set(ctx, state, "arena", arena);

    let r = Table::new(&ctx);
    set(ctx, r, "ticks_per_sec", rules::TICKS_PER_SEC as i64);
    set(ctx, r, "max_ticks", rules::MAX_TICKS as i64);
    set(ctx, r, "walk_speed", rules::WALK_SPEED as f64);
    set(ctx, r, "attack_range", rules::ATTACK_RANGE as f64);
    set(ctx, r, "attack_damage", rules::ATTACK_DAMAGE as i64);
    set(
        ctx,
        r,
        "attack_cooldown",
        rules::ATTACK_COOLDOWN_TICKS as i64,
    );
    set(ctx, r, "max_hp", rules::MAX_HP as i64);
    set(ctx, state, "rules", r);

    state
}

fn seen_table<'gc>(ctx: Context<'gc>, seen: &Seen) -> Table<'gc> {
    let table = Table::new(&ctx);
    set(ctx, table, "id", seen.id as i64);
    set(ctx, table, "x", seen.x as f64);
    set(ctx, table, "y", seen.y as f64);
    set(ctx, table, "hp", seen.hp as i64);
    table
}

fn set<'gc>(
    ctx: Context<'gc>,
    table: Table<'gc>,
    key: &'static str,
    value: impl piccolo::IntoValue<'gc>,
) {
    // Setting a string key to a non-NaN value cannot fail.
    let _ = table.set(ctx, key, value);
}

fn parse_action<'gc>(ctx: Context<'gc>, value: Value<'gc>) -> Result<Action, String> {
    let (name, args) = match value {
        Value::Nil => return Ok(Action::Idle),
        // `return "attack"` is accepted as shorthand for `{ action = "attack" }`.
        Value::String(name) => (name, None),
        Value::Table(table) => match table.get(ctx, "action") {
            Value::String(name) => (name, Some(table)),
            other => {
                return Err(format!(
                    "tick() returned a table whose `action` is {}, expected a string",
                    other.type_name()
                ));
            }
        },
        other => {
            return Err(format!(
                "tick() must return a table or nil, got {}",
                other.type_name()
            ));
        }
    };

    match name.as_bytes() {
        b"idle" => Ok(Action::Idle),
        b"attack" => Ok(Action::Attack),
        b"walk" => {
            let axis = |key: &'static str| {
                args.and_then(|table| table.get(ctx, key).to_number())
                    .unwrap_or(0.0) as f32
            };
            let (dx, dy) = (axis("dx"), axis("dy"));
            if dx.is_finite() && dy.is_finite() {
                Ok(Action::Walk { dx, dy })
            } else {
                Err("walk direction must be finite numbers".into())
            }
        }
        _ => Err(format!(
            "unknown action \"{}\" (expected walk, attack or idle)",
            name.to_str_lossy()
        )),
    }
}
