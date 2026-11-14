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

/// A pad the room's owner placed. The pads form a ring by number: a bot that
/// gets onto one comes out of the next, and the last leads back to the first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Teleporter {
    /// The number players see on the pad, and what scripts call it.
    pub id: u32,
    pub x: f32,
    pub y: f32,
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
    /// Whether a pad takes the bot. Not after a jump, until it has stepped
    /// off the pad it came out of: otherwise it would never stop jumping.
    teleports: bool,
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

/// The pad a bot comes out of when it gets onto pad `from`: the next one by
/// number, and the first one after the last. `None` for a lone pad, which
/// leads nowhere. `teleporters` is sorted by id.
pub fn exit_pad(teleporters: &[Teleporter], from: usize) -> Option<Teleporter> {
    (teleporters.len() > 1).then(|| teleporters[(from + 1) % teleporters.len()])
}

/// Pads take every living bot that is on them, whether it walked there or
/// was shoved, and put it on the exact centre of the pad they lead to.
fn teleport(bots: &mut [Bot], teleporters: &[Teleporter]) {
    for bot in bots.iter_mut().filter(|bot| bot.alive()) {
        // Pads never overlap, so a bot is on one at most.
        let on = teleporters.iter().position(|pad| {
            (pad.x - bot.x).powi(2) + (pad.y - bot.y).powi(2) < rules::TELEPORTER_RADIUS.powi(2)
        });
        match on.map(|from| exit_pad(teleporters, from)) {
            None => bot.teleports = true,
            Some(Some(exit)) if bot.teleports => {
                (bot.x, bot.y) = (exit.x, exit.y);
                bot.teleports = false;
            }
            Some(_) => {}
        }
    }
}

/// Simulate a full match of at most `max_ticks` ticks. Deterministic for a
/// given `(specs, teleporters, seed, max_ticks)`. `teleporters` in any order.
pub fn run_match(
    specs: &[BotSpec],
    teleporters: &[Teleporter],
    seed: u64,
    max_ticks: u32,
) -> MatchOutcome {
    let mut teleporters = teleporters.to_vec();
    teleporters.sort_by_key(|pad| pad.id);
    let teleporters = &teleporters[..];

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
                teleports: true,
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
            .map(|i| decide(&mut bots, teleporters, i, tick, max_ticks, &mut faults))
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
            if !bot.alive() {
                continue;
            }
            let step = bot.stats.walk_speed / rules::TICKS_PER_SEC as f32;
            match actions[i] {
                Action::Walk { dx, dy } => {
                    let len = (dx * dx + dy * dy).sqrt();
                    if len > 1e-6 {
                        bot.x = (bot.x + dx / len * step).clamp(-limit, limit);
                        bot.y = (bot.y + dy / len * step).clamp(-limit, limit);
                        did[i] = ActionKind::Walk;
                    }
                }
                Action::Idle | Action::Attack => {}
            }
        }
        separate(&mut bots, limit);
        // After the shoving, so a bot can be pushed onto a pad.
        teleport(&mut bots, teleporters);

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
    teleporters: &[Teleporter],
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
        teleporters,
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
        super::run_match(specs, &[], seed, MAX_TICKS)
    }

    fn pad(id: u32, x: f32, y: f32) -> Teleporter {
        Teleporter { id, x, y }
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
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &[], 0, 1);
        assert!(out.faults.is_empty(), "{:?}", out.faults);
        assert_eq!(out.frames[0].bots[0].hp, 160);
        assert!((out.frames[1].bots[0].x - 0.42).abs() < 0.001);
    }

    #[test]
    fn build_selection_matches_preview_for_every_match_seed() {
        let script = "build={hp=math.random(0,5),speed=0,damage=0,range=0,cooldown=0} function tick(s) build.hp=5 end";
        let preview = LuaBot::new(script, 0).unwrap().stats;
        for seed in [1, 42, 999] {
            let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &[], seed, 2);
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
        let outcome = super::run_match(&[spec(1, script), spec(2, IDLE)], &[], 1, 150);
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
        let outcome = super::run_match(&specs, &[], 0, 1);
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

    const WALK_DOWN: &str =
        r#"function tick(state) return { action = "walk", dx = 0, dy = -1 } end"#;

    fn path(out: &MatchOutcome, bot: usize) -> Vec<(f32, f32)> {
        out.frames
            .iter()
            .map(|f| (f.bots[bot].x, f.bots[bot].y))
            .collect()
    }

    #[test]
    fn a_bot_that_walks_onto_a_pad_comes_out_of_the_next() {
        // Bot 1 starts at (0, 8.5). Pad 1 is two units in front of it.
        let pads = [pad(1, 0.0, 6.5), pad(2, -5.0, -5.0)];
        let out = super::run_match(&[spec(1, WALK_DOWN), spec(2, IDLE)], &pads, 0, 8);
        assert!(out.faults.is_empty(), "{:?}", out.faults);

        let path = path(&out, 0);
        // The pad's edge is 1.1 units away: three steps of 0.3 stop short of
        // it, and the fourth is on the pad.
        assert!(path[3].0 == 0.0 && (path[3].1 - 7.6).abs() < 1e-4);
        assert_eq!(path[4], (-5.0, -5.0));
        // Then it walks off the pad it came out of, and is not sent back.
        assert!((path[8].1 - -6.2).abs() < 1e-4 && path[8].0 == -5.0);
    }

    #[test]
    fn pads_lead_to_the_next_number_and_the_last_to_the_first() {
        // Handed over out of order, with a gap in the numbers.
        let pads = [pad(7, 5.0, 5.0), pad(2, -5.0, 5.0), pad(4, -5.0, -5.0)];
        let sorted = [pads[1], pads[2], pads[0]];
        assert_eq!(exit_pad(&sorted, 0), Some(sorted[1]));
        assert_eq!(exit_pad(&sorted, 1), Some(sorted[2]));
        assert_eq!(exit_pad(&sorted, 2), Some(sorted[0]));

        // Walks to the pad it is told to, and stands still once it has jumped.
        for (enter, exit) in [(2, (-5.0, -5.0)), (4, (5.0, 5.0)), (7, (-5.0, 5.0))] {
            let script = format!(
                r#"
                local last, jumped
                function tick(state)
                  local me = state.me
                  if last and math.abs(me.x - last.x) + math.abs(me.y - last.y) > 1 then
                    jumped = true
                  end
                  last = me
                  if jumped then return {{ action = "idle" }} end
                  for _, pad in ipairs(state.teleporters) do
                    if pad.id == {enter} then
                      return {{ action = "walk", dx = pad.x - me.x, dy = pad.y - me.y }}
                    end
                  end
                end
                "#
            );
            let out = super::run_match(&[spec(1, &script), spec(2, IDLE)], &pads, 0, 80);
            assert!(out.faults.is_empty(), "{:?}", out.faults);
            let last = out.frames.last().unwrap().bots[0];
            assert_eq!((last.x, last.y), exit, "entering {enter}");
        }
    }

    #[test]
    fn a_bot_shoved_onto_a_pad_teleports() {
        // Bot 2 never moves by itself. It starts at (0, -8.5), just off pad
        // 1, and bot 1 walks into it and pushes it against the railing.
        let pads = [pad(1, 0.7, -9.1), pad(2, -5.0, -5.0)];
        let chase = r#"
            function tick(state)
              local e = state.enemies[1]
              return { action = "walk", dx = e.x - state.me.x, dy = e.y - state.me.y }
            end
        "#;
        let out = super::run_match(&[spec(1, chase), spec(2, IDLE)], &pads, 0, 60);
        assert!(out.faults.is_empty(), "{:?}", out.faults);
        let shoved = path(&out, 1);
        let jump = shoved.iter().position(|&at| at == (-5.0, -5.0));
        assert!(jump.is_some(), "{shoved:?}");
        // The bot that did the shoving stayed behind.
        let pusher = path(&out, 0)[jump.unwrap()];
        assert!(pusher.0 == 0.0 && pusher.1 < -7.0, "{pusher:?}");
    }

    #[test]
    fn a_bot_does_not_jump_again_until_it_steps_off() {
        let pads = [pad(1, 0.0, 6.5), pad(2, -5.0, -5.0)];
        // Walks onto pad 1, then stays put on pad 2.
        let script = r#"
            function tick(state)
              if state.me.y > 0 then return { action = "walk", dx = 0, dy = -1 } end
            end
        "#;
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &pads, 0, 40);
        assert!(out.faults.is_empty(), "{:?}", out.faults);
        let stayed = path(&out, 0);
        assert!(
            stayed[4..].iter().all(|&at| at == (-5.0, -5.0)),
            "{stayed:?}"
        );

        // Steps off pad 2 and back on: that takes it to pad 1 again.
        let script = r#"
            local out = false
            function tick(state)
              if state.me.y > 0 then return { action = "walk", dx = 0, dy = -1 } end
              local pad = state.teleporters[2]
              if pad.dist > 1.2 then out = true end
              return { action = "walk", dx = out and 1 or -1, dy = 0 }
            end
        "#;
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &pads, 0, 40);
        assert!(out.faults.is_empty(), "{:?}", out.faults);
        let back = path(&out, 0);
        assert!(back[5..].contains(&(0.0, 6.5)), "{back:?}");
    }

    #[test]
    fn a_lone_pad_leads_nowhere() {
        let pads = [pad(1, 0.0, 6.5)];
        let out = super::run_match(&[spec(1, WALK_DOWN), spec(2, IDLE)], &pads, 0, 8);
        let last = out.frames[8].bots[0];
        assert!((last.y - 6.1).abs() < 1e-4 && last.x == 0.0, "{last:?}");
    }

    #[test]
    fn scripts_see_the_teleporters() {
        let pads = [pad(4, 0.0, 6.5), pad(7, -5.0, -5.0)];
        let script = r#"
            function tick(state)
              local t = state.teleporters
              assert(#t == 2 and t[1].id == 4 and t[2].id == 7)
              assert(t[1].to == 7 and t[2].to == 4)
              assert(t[2].x == -5 and t[2].y == -5)
              assert(math.abs(t[1].dist - 2) < 1e-4)
              assert(math.abs(state.rules.teleporter_radius - 0.9) < 1e-6)
            end
        "#;
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &pads, 0, 1);
        assert!(out.faults.is_empty(), "{:?}", out.faults);

        let script = "function tick(state) assert(state.teleporters[1].to == nil) end";
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &pads[..1], 0, 1);
        assert!(out.faults.is_empty(), "{:?}", out.faults);
    }

    #[test]
    fn teleport_is_not_an_action() {
        let script = "function tick() return { action = 'teleport', to = 2 } end";
        let out = super::run_match(&[spec(1, script), spec(2, IDLE)], &[], 0, 1);
        assert!(out.faults[0].message.contains("unknown action"));
    }
}
