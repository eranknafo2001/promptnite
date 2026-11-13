//! The match loop: spawn bots, ask every script for an action each tick,
//! apply the rules, record a frame.
//!
//! A match takes no player input once it starts, so the whole thing is
//! simulated in one call and returned as a list of frames for clients to
//! replay.

use crate::{
    lua::{BotView, LuaBot, Seen},
    rules,
};

/// One participant: who they are, where they start and the Lua their agent
/// wrote.
pub struct BotSpec {
    pub id: u32,
    /// Which edge of the arena the bot starts at, see `rules::edge_direction`.
    /// No two bots of a match should share one.
    pub edge: u32,
    pub script: String,
}

/// What a script asked for on one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Idle,
    /// Direction only; the length is ignored (normalised by the rules).
    Walk {
        dx: f32,
        dy: f32,
    },
    Attack,
}

/// What a bot actually did on a tick, for rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Idle,
    Walk,
    /// Swung and hit somebody.
    Attack,
    /// Swung at nothing (out of range or on cooldown).
    Miss,
    Dead,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BotSnapshot {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub hp: i32,
    pub action: ActionKind,
}

/// State of the arena *after* tick `tick` was applied. Frame 0 is the spawn.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub tick: u32,
    pub bots: Vec<BotSnapshot>,
}

/// A script problem worth showing to its author.
#[derive(Clone, Debug, PartialEq)]
pub struct Fault {
    pub bot_id: u32,
    pub tick: u32,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchOutcome {
    pub frames: Vec<Frame>,
    /// `None` on a draw (everyone dead, or tied HP at the time limit).
    pub winner: Option<u32>,
    /// First fault of each misbehaving bot.
    pub faults: Vec<Fault>,
}

struct Bot {
    stats: rules::Stats,
    id: u32,
    x: f32,
    y: f32,
    hp: i32,
    cooldown: u32,
    /// `None` once the script is disabled (failed to load, or blew the heap cap).
    lua: Option<LuaBot>,
    faulted: bool,
}

impl Bot {
    fn alive(&self) -> bool {
        self.hp > 0
    }
}

/// Bots are solid: push living bots that overlap apart, half the overlap each.
/// One pass is enough for a placeholder; a crowd may still overlap slightly.
fn separate(bots: &mut [Bot], limit: f32) {
    let min_dist = rules::BOT_RADIUS * 2.0;
    for i in 0..bots.len() {
        for j in i + 1..bots.len() {
            if !bots[i].alive() || !bots[j].alive() {
                continue;
            }
            let (dx, dy) = (bots[j].x - bots[i].x, bots[j].y - bots[i].y);
            let dist = (dx * dx + dy * dy).sqrt();
            if dist >= min_dist {
                continue;
            }
            // Exactly on top of each other: pick a fixed axis to stay deterministic.
            let (nx, ny) = if dist > 1e-6 {
                (dx / dist, dy / dist)
            } else {
                (1.0, 0.0)
            };
            let push = (min_dist - dist) / 2.0;
            bots[i].x = (bots[i].x - nx * push).clamp(-limit, limit);
            bots[i].y = (bots[i].y - ny * push).clamp(-limit, limit);
            bots[j].x = (bots[j].x + nx * push).clamp(-limit, limit);
            bots[j].y = (bots[j].y + ny * push).clamp(-limit, limit);
        }
    }
}

/// Simulate a full match of at most `max_ticks` ticks. Deterministic for a
/// given `(specs, seed, max_ticks)`.
pub fn run_match(specs: &[BotSpec], seed: u64, max_ticks: u32) -> MatchOutcome {
    let mut faults = Vec::new();
    let spawn_dist = rules::ARENA_HALF_SIZE - rules::SPAWN_INSET;

    let mut bots: Vec<Bot> = specs
        .iter()
        .map(|spec| {
            let (dx, dy) = rules::edge_direction(spec.edge);
            let lua = match LuaBot::new(&spec.script, seed.wrapping_add(spec.id as u64)) {
                Ok(lua) => Some(lua),
                Err(message) => {
                    faults.push(Fault {
                        bot_id: spec.id,
                        tick: 0,
                        message,
                    });
                    None
                }
            };
            let stats = lua.as_ref().map(|l| l.stats).unwrap_or_default();
            Bot {
                stats,
                id: spec.id,
                x: dx * spawn_dist,
                y: dy * spawn_dist,
                hp: stats.max_hp,
                cooldown: 0,
                faulted: lua.is_none(),
                lua,
            }
        })
        .collect();

    let mut frames = Vec::with_capacity(max_ticks as usize + 1);
    frames.push(snapshot(0, &bots, &vec![ActionKind::Idle; bots.len()]));

    for tick in 1..=max_ticks {
        if bots.iter().filter(|b| b.alive()).count() <= 1 {
            break;
        }

        // 1. Every bot decides from the same snapshot, so order doesn't matter.
        let actions: Vec<Action> = (0..bots.len())
            .map(|i| decide(&mut bots, i, tick, max_ticks, &mut faults))
            .collect();

        let mut did: Vec<ActionKind> = bots
            .iter()
            .map(|b| {
                if b.alive() {
                    ActionKind::Idle
                } else {
                    ActionKind::Dead
                }
            })
            .collect();

        // 2. Movement.

        let limit = rules::BOT_LIMIT;
        for (i, bot) in bots.iter_mut().enumerate() {
            let step = bot.stats.walk_speed / rules::TICKS_PER_SEC as f32;
            if let Action::Walk { dx, dy } = actions[i] {
                let len = (dx * dx + dy * dy).sqrt();
                if bot.alive() && len > 1e-6 {
                    bot.x = (bot.x + dx / len * step).clamp(-limit, limit);
                    bot.y = (bot.y + dy / len * step).clamp(-limit, limit);
                    did[i] = ActionKind::Walk;
                }
            }
        }
        separate(&mut bots, limit);

        // 3. Attacks, resolved simultaneously: two bots can knock each other out.
        let mut damage = vec![0; bots.len()];
        for i in 0..bots.len() {
            if !bots[i].alive() {
                continue;
            }
            if actions[i] == Action::Attack && bots[i].cooldown == 0 {
                bots[i].cooldown = bots[i].stats.attack_cooldown;
                match nearest_enemy(&bots, i) {
                    Some((target, dist)) if dist <= bots[i].stats.attack_range => {
                        damage[target] += bots[i].stats.attack_damage;
                        did[i] = ActionKind::Attack;
                    }
                    _ => did[i] = ActionKind::Miss,
                }
            } else {
                if actions[i] == Action::Attack {
                    did[i] = ActionKind::Miss;
                }
                bots[i].cooldown = bots[i].cooldown.saturating_sub(1);
            }
        }
        for (i, bot) in bots.iter_mut().enumerate() {
            if bot.alive() && damage[i] > 0 {
                bot.hp = (bot.hp - damage[i]).max(0);
                if !bot.alive() {
                    did[i] = ActionKind::Dead;
                }
            }
        }

        frames.push(snapshot(tick, &bots, &did));
    }

    MatchOutcome {
        winner: pick_winner(&bots),
        frames,
        faults,
    }
}

fn decide(
    bots: &mut [Bot],
    i: usize,
    tick: u32,
    max_ticks: u32,
    faults: &mut Vec<Fault>,
) -> Action {
    if !bots[i].alive() || bots[i].lua.is_none() {
        return Action::Idle;
    }

    let me = &bots[i];
    let enemies: Vec<Seen> = bots
        .iter()
        .enumerate()
        .filter(|(j, other)| *j != i && other.alive())
        .map(|(_, other)| Seen {
            stats: other.stats,
            id: other.id,
            x: other.x,
            y: other.y,
            hp: other.hp,
            dist: distance(me, other),
        })
        .collect();
    let view = BotView {
        tick,
        max_ticks,
        me: Seen {
            stats: me.stats,
            id: me.id,
            x: me.x,
            y: me.y,
            hp: me.hp,
            dist: 0.0,
        },
        cooldown: me.cooldown,
        enemies: &enemies,
    };

    let bot = &mut bots[i];
    let lua = bot.lua.as_mut().expect("checked above");
    let result = lua.tick(&view);
    if lua.out_of_memory() {
        // Not a lost turn: the script is switched off for the rest of the match.
        bot.lua = None;
    }

    result.unwrap_or_else(|message| {
        // A broken tick just loses the turn. Report only the first one per bot.
        if !bot.faulted {
            bot.faulted = true;
            faults.push(Fault {
                bot_id: bot.id,
                tick,
                message,
            });
        }
        Action::Idle
    })
}

fn distance(a: &Bot, b: &Bot) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Index and distance of the closest living bot other than `i`.
fn nearest_enemy(bots: &[Bot], i: usize) -> Option<(usize, f32)> {
    bots.iter()
        .enumerate()
        .filter(|(j, other)| *j != i && other.alive())
        .map(|(j, other)| (j, distance(&bots[i], other)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

fn snapshot(tick: u32, bots: &[Bot], did: &[ActionKind]) -> Frame {
    Frame {
        tick,
        bots: bots
            .iter()
            .zip(did)
            .map(|(bot, &action)| BotSnapshot {
                id: bot.id,
                x: bot.x,
                y: bot.y,
                hp: bot.hp,
                action,
            })
            .collect(),
    }
}

/// Last bot standing, otherwise the unique highest HP, otherwise a draw.
fn pick_winner(bots: &[Bot]) -> Option<u32> {
    let best = bots.iter().map(|b| b.hp).max()?;
    if best <= 0 {
        return None;
    }
    let mut leaders = bots.iter().filter(|b| b.hp == best);
    let leader = leaders.next()?;
    leaders.next().is_none().then_some(leader.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EXAMPLE_SCRIPT, validate_script};

    fn spec(id: u32, script: &str) -> BotSpec {
        BotSpec {
            id,
            edge: id - 1,
            script: script.to_string(),
        }
    }

    const MAX_TICKS: u32 = rules::DEFAULT_MATCH_SECS * rules::TICKS_PER_SEC;

    fn run_match(specs: &[BotSpec], seed: u64) -> MatchOutcome {
        super::run_match(specs, seed, MAX_TICKS)
    }

    const IDLE: &str = "function tick(state) return nil end";

    #[test]
    fn loadouts_are_validated_and_applied() {
        for build in [
            "{hp=5,speed=5,damage=5,range=0,cooldown=0}",
            "{hp=-1}",
            "{hp=0/0}",
        ] {
            assert!(validate_script(&format!("build={build} function tick() end")).is_err());
        }
        let script = "build={hp=5,speed=5,damage=0,range=0,cooldown=0} function tick(s) assert(s.me.max_hp==160 and s.rules.attack_damage==6 and s.enemies[1].max_hp==100) return {action='walk',dx=1,dy=0} end";
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], 0, 1);
        assert!(out.faults.is_empty(), "{:?}", out.faults);
        assert_eq!(out.frames[0].bots[0].hp, 160);
        assert!((out.frames[1].bots[0].x - 0.42).abs() < 0.001);
    }

    #[test]
    fn build_selection_matches_preview_for_every_match_seed() {
        let script = "build={hp=math.random(0,5),speed=0,damage=0,range=0,cooldown=0} function tick(s) build.hp=5 end";
        let preview = LuaBot::new(script, 0).unwrap().stats;
        for seed in [1, 42, 999] {
            let out = super::run_match(&[spec(1, script), spec(2, IDLE)], seed, 2);
            assert_eq!(out.frames[0].bots[0].hp, preview.max_hp);
            assert_eq!(out.frames.last().unwrap().bots[0].hp, preview.max_hp);
        }
    }

    #[test]
    fn example_script_is_valid() {
        validate_script(EXAMPLE_SCRIPT).unwrap();
    }

    #[test]
    fn rejects_scripts_without_tick() {
        let err = validate_script("local x = 1").unwrap_err();
        assert!(err.contains("tick"), "{err}");
    }

    #[test]
    fn rejects_syntax_errors() {
        assert!(validate_script("function tick(").is_err());
    }

    #[test]
    fn rejects_runaway_top_level_code() {
        let err = validate_script("while true do end").unwrap_err();
        assert!(err.contains("instructions"), "{err}");
    }

    #[test]
    fn sandbox_has_no_io_or_os() {
        validate_script("assert(io == nil and os == nil) function tick() end").unwrap();
    }

    #[test]
    fn chaser_beats_idle_bot() {
        let outcome = run_match(&[spec(1, EXAMPLE_SCRIPT), spec(2, IDLE)], 42);
        assert_eq!(outcome.winner, Some(1));
        assert!(outcome.faults.is_empty(), "{:?}", outcome.faults);
        let last = outcome.frames.last().unwrap();
        assert_eq!(last.bots[1].hp, 0);
        assert_eq!(last.bots[1].action, ActionKind::Dead);
        assert!((last.tick as usize) < MAX_TICKS as usize);
    }

    #[test]
    fn idle_bots_draw_at_the_time_limit() {
        let outcome = run_match(&[spec(1, IDLE), spec(2, IDLE)], 1);
        assert_eq!(outcome.winner, None);
        assert_eq!(outcome.frames.len(), MAX_TICKS as usize + 1);
    }

    #[test]
    fn match_length_is_configurable() {
        let script = "function tick(state) assert(state.rules.max_ticks == 150) end";
        let outcome = super::run_match(&[spec(1, script), spec(2, IDLE)], 1, 150);
        assert!(outcome.faults.is_empty(), "{:?}", outcome.faults);
        assert_eq!(outcome.frames.len(), 151);
    }

    #[test]
    fn matches_are_deterministic() {
        let wanderer = r#"
            function tick(state)
              return { action = "walk", dx = math.random() - 0.5, dy = math.random() - 0.5 }
            end
        "#;
        let bots = [
            spec(1, wanderer),
            spec(2, EXAMPLE_SCRIPT),
            spec(3, wanderer),
        ];
        assert_eq!(run_match(&bots, 7), run_match(&bots, 7));
        assert_ne!(run_match(&bots, 7).frames, run_match(&bots, 8).frames);
    }

    #[test]
    fn infinite_loop_costs_the_turn_not_the_match() {
        let outcome = run_match(
            &[
                spec(1, "function tick() while true do end end"),
                spec(2, EXAMPLE_SCRIPT),
            ],
            3,
        );
        assert_eq!(outcome.winner, Some(2));
        assert_eq!(outcome.faults.len(), 1);
        assert_eq!(outcome.faults[0].bot_id, 1);
    }

    #[test]
    fn broken_script_still_gets_a_bot() {
        let outcome = run_match(&[spec(1, "this is not lua"), spec(2, EXAMPLE_SCRIPT)], 3);
        assert_eq!(outcome.winner, Some(2));
        assert_eq!(outcome.faults[0].tick, 0);
    }

    #[test]
    fn scripts_keep_state_between_ticks() {
        // Walks left for 10 ticks, then stops: needs a counter that persists.
        let script = r#"
            local n = 0
            function tick(state)
              n = n + 1
              if n <= 10 then return { action = "walk", dx = -1, dy = 0 } end
            end
        "#;
        let outcome = run_match(&[spec(1, script), spec(2, IDLE)], 0);
        let start = outcome.frames[0].bots[0].x;
        let end = outcome.frames.last().unwrap().bots[0].x;
        let expected = 10.0 * rules::WALK_SPEED / rules::TICKS_PER_SEC as f32;
        assert!(
            (start - end - expected).abs() < 1e-3,
            "moved {}",
            start - end
        );
    }

    #[test]
    fn bots_stay_inside_the_arena() {
        let runner = r#"function tick() return { action = "walk", dx = 1, dy = 1 } end"#;
        let outcome = run_match(&[spec(1, runner), spec(2, IDLE)], 0);
        let limit = rules::BOT_LIMIT;
        for frame in &outcome.frames {
            assert!(frame.bots[0].x <= limit && frame.bots[0].y <= limit);
        }
    }

    #[test]
    fn string_shorthand_and_cooldown() {
        // Always attacking: only every (cooldown + 1)th swing is real.
        let puncher = r#"function tick() return "attack" end"#;
        let outcome = run_match(&[spec(1, puncher), spec(2, IDLE)], 0);
        assert!(outcome.faults.is_empty(), "{:?}", outcome.faults);
        // Never in range (spawned on opposite edges), so nobody takes damage.
        assert_eq!(outcome.frames.last().unwrap().bots[1].hp, rules::MAX_HP);
        assert_eq!(outcome.frames[1].bots[0].action, ActionKind::Miss);
    }

    #[test]
    fn bots_start_at_their_own_edge() {
        let specs: Vec<BotSpec> = (1..=4).map(|id| spec(id, IDLE)).collect();
        let outcome = super::run_match(&specs, 0, 1);
        let d = rules::ARENA_HALF_SIZE - rules::SPAWN_INSET;
        let at: Vec<(f32, f32)> = outcome.frames[0].bots.iter().map(|b| (b.x, b.y)).collect();
        assert_eq!(at, [(0.0, d), (0.0, -d), (d, 0.0), (-d, 0.0)]);
    }

    #[test]
    fn living_bots_do_not_overlap() {
        // Two chasers walk straight at each other and would otherwise merge.
        let outcome = run_match(
            &[
                spec(1, crate::EXAMPLE_SCRIPT),
                spec(2, crate::EXAMPLE_SCRIPT),
            ],
            1,
        );
        for frame in &outcome.frames {
            let (a, b) = (&frame.bots[0], &frame.bots[1]);
            if a.hp > 0 && b.hp > 0 {
                let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                assert!(
                    dist >= rules::BOT_RADIUS * 2.0 - 1e-3,
                    "tick {}: {dist}",
                    frame.tick
                );
            }
        }
    }
}
