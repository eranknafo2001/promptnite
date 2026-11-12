//! Sandboxed Lua runner for one bot.
//!
//! Scripts are untrusted (an LLM wrote them from a player's prompt), so each
//! bot gets its own VM with:
//!   * only the pure parts of the stdlib (no `io`, no `os`),
//!   * a work budget per call (see [`rules::TICK_FUEL`]),
//!   * a heap cap (see [`rules::MAX_LUA_MEMORY_BYTES`]),
//!   * a seeded `math.random`, so a match replays identically.
//!
//! The module that runs this has no clock, so "how long may a tick take" has
//! to be answered by counting. Three things together bound it:
//!
//!   1. Fuel: the VM charges for every instruction and call.
//!   2. No way to build a big value in a few instructions. The only one Lua
//!      offers is string concatenation (`s = s .. s` doubles a string each
//!      time, a gigabyte in 30 instructions), so scripts that use `..` or
//!      carry a long string literal are refused when they are loaded
//!      ([`check_prototype`]). Everything else grows by a bounded amount per
//!      instruction.
//!   3. Fuel costs more the more memory a script holds
//!      ([`rules::FUEL_MEMORY_UNIT`]). One instruction can be as expensive as
//!      the heap is large (walking a table, copying a string), so this keeps
//!      the real work per tick bounded, not just the instruction count.

use piccolo::{
    Callback, CallbackReturn, Closure, Context, Executor, Fuel, FunctionPrototype, IntoValue, Lua,
    StashedExecutor, StashedFunction, Table, Value, constant::Constant, opcode::Operation, stdlib,
};

use crate::{rules, sim::Action};

/// What a bot can see when it decides its action.
pub struct BotView<'a> {
    pub tick: u32,
    /// The match's time limit, in ticks.
    pub max_ticks: u32,
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
    /// Heap size of the VM before the script was loaded.
    floor: usize,
    /// Set once the script has gone over the heap cap.
    out_of_memory: bool,
}

/// Why a script was stopped before it finished.
enum Stop {
    Fuel,
    Memory,
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
            bound_unpack(ctx);
        });
        let executor = lua.enter(|ctx| ctx.stash(Executor::new(ctx)));
        lua.gc_collect();
        let floor = lua.total_memory();

        // Seed first so the player's own top-level code already sees a
        // deterministic RNG. Kept as a separate chunk so error line numbers
        // in the player's script stay correct.
        run_chunk(
            &mut lua,
            &executor,
            floor,
            "seed",
            &format!("math.randomseed({seed})"),
        )?;
        run_chunk(&mut lua, &executor, floor, "bot", source)?;

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
            floor,
            out_of_memory: false,
        })
    }

    /// Ask the script for this tick's action.
    ///
    /// An `Err` means the script misbehaved (runtime error, budget exceeded,
    /// nonsense return value). The caller decides what that costs the bot.
    pub fn tick(&mut self, view: &BotView) -> Result<Action, String> {
        let (executor, tick_fn) = (&self.executor, &self.tick_fn);

        self.lua.enter(|ctx| {
            let state = build_state(ctx, view);
            ctx.fetch(executor).restart(ctx, ctx.fetch(tick_fn), state);
        });
        match run(&mut self.lua, executor, self.floor, rules::TICK_FUEL) {
            Ok(()) => {}
            Err(Stop::Fuel) => {
                return Err(format!(
                    "tick() ran out of its budget of {} instructions \
                     (they cost more the more memory the script holds)",
                    rules::TICK_FUEL
                ));
            }
            Err(Stop::Memory) => {
                self.out_of_memory = true;
                return Err(out_of_memory());
            }
        }

        self.lua
            .enter(|ctx| match ctx.fetch(executor).take_result::<Value>(ctx) {
                Ok(Ok(value)) => parse_action(ctx, value),
                Ok(Err(err)) => Err(err.to_string()),
                Err(err) => Err(err.to_string()),
            })
    }

    /// Current Lua heap size in bytes, garbage included.
    pub fn memory(&self) -> usize {
        self.lua.total_memory()
    }

    /// Whether a tick went over [`rules::MAX_LUA_MEMORY_BYTES`]. Such a
    /// script is not worth calling again.
    pub fn out_of_memory(&self) -> bool {
        self.out_of_memory
    }
}

/// Replace `table.unpack` with one that cannot be asked for more values than
/// [`rules::MAX_UNPACK_VALUES`], and that pays fuel for each. The stock one
/// takes the range on trust: `table.unpack({}, 1, 1e9)` makes room for a
/// billion values in a single call.
fn bound_unpack(ctx: Context<'_>) {
    let unpack = Callback::from_fn(&ctx, |ctx, mut exec, mut stack| {
        let (table, start, end): (Table, Option<i64>, Option<i64>) = stack.consume(ctx)?;
        let start = start.unwrap_or(1);
        let end = end.unwrap_or_else(|| table.length());
        if start <= end {
            let count = end
                .checked_sub(start)
                .filter(|span| *span < rules::MAX_UNPACK_VALUES as i64)
                .ok_or_else(|| "too many results to unpack".into_value(ctx))?
                + 1;
            exec.fuel().consume(count as i32);
            stack.extend((start..=end).map(|i| table.get_value(i.into())));
        }
        Ok(CallbackReturn::Return)
    });
    if let Value::Table(table) = ctx.get_global("table") {
        table.set(ctx, "unpack", unpack).unwrap();
    }
}

fn out_of_memory() -> String {
    format!(
        "script used more than {} bytes of memory",
        rules::MAX_LUA_MEMORY_BYTES
    )
}

/// Run whatever the executor was last restarted with, on a budget.
///
/// The VM is stepped a slice at a time. After each slice the heap is checked
/// against the cap and the slice is paid for: one unit of fuel per unit the
/// VM charged, times one more for every [`rules::FUEL_MEMORY_UNIT`] bytes the
/// script holds.
fn run(lua: &mut Lua, executor: &StashedExecutor, floor: usize, budget: i32) -> Result<(), Stop> {
    let mut left = i64::from(budget);
    loop {
        let slice = left.clamp(1, i64::from(rules::FUEL_SLICE)) as i32;
        let mut fuel = Fuel::with(slice);
        let finished = lua.enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel));
        // A slice may overrun a little; `remaining` is negative then.
        let used = i64::from(slice) - i64::from(fuel.remaining());

        let memory = lua.total_memory();
        if memory > rules::MAX_LUA_MEMORY_BYTES {
            // Only what is still reachable counts against the cap.
            lua.gc_collect();
            if lua.total_memory() > rules::MAX_LUA_MEMORY_BYTES {
                return Err(Stop::Memory);
            }
        }
        let held = memory.saturating_sub(floor) / rules::FUEL_MEMORY_UNIT;
        left = left.saturating_sub(used.saturating_mul(1 + held as i64));

        if finished {
            return Ok(());
        }
        if left <= 0 {
            return Err(Stop::Fuel);
        }
    }
}

/// Refuse compiled code that could build a huge string in a few instructions:
/// any use of `..`, and string literals longer than
/// [`rules::MAX_STRING_LITERAL_BYTES`]. With those gone, no string a script
/// can get hold of is longer than that limit.
fn check_prototype(prototype: &FunctionPrototype) -> Result<(), String> {
    for constant in prototype.constants.iter() {
        if let Constant::String(string) = constant
            && string.as_bytes().len() > rules::MAX_STRING_LITERAL_BYTES
        {
            return Err(format!(
                "string literals may be at most {} bytes long",
                rules::MAX_STRING_LITERAL_BYTES
            ));
        }
    }
    for opcode in prototype.opcodes.iter() {
        if let Operation::Concat { .. } = opcode.decode() {
            return Err("string concatenation (`..`) is not available to bots".into());
        }
    }
    prototype
        .prototypes
        .iter()
        .try_for_each(|nested| check_prototype(nested))
}

/// Check that a script compiles, loads within budget and defines `tick`.
/// Used by the server to reject a bad script before the round starts.
pub fn validate_script(source: &str) -> Result<(), String> {
    LuaBot::new(source, 0).map(|_| ())
}

fn run_chunk(
    lua: &mut Lua,
    executor: &StashedExecutor,
    floor: usize,
    name: &str,
    source: &str,
) -> Result<(), String> {
    lua.enter(|ctx| {
        let closure =
            Closure::load(ctx, Some(name), source.as_bytes()).map_err(|err| err.to_string())?;
        check_prototype(&closure.prototype())?;
        ctx.fetch(executor).restart(ctx, closure.into(), ());
        Ok::<(), String>(())
    })?;

    match run(lua, executor, floor, rules::INIT_FUEL) {
        Ok(()) => {}
        Err(Stop::Fuel) => {
            return Err(format!(
                "top-level code ran out of its budget of {} instructions",
                rules::INIT_FUEL
            ));
        }
        Err(Stop::Memory) => return Err(out_of_memory()),
    }

    lua.enter(|ctx| match ctx.fetch(executor).take_result::<Value>(ctx) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(err) => Err(err.to_string()),
    })
}

/// The `state` table handed to `tick`. Keep this in sync with the agent's
/// system prompt (`worker/src/llm.ts`).
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
    set(ctx, r, "max_ticks", view.max_ticks as i64);
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
