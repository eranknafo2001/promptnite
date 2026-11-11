# Promptnite — game design

## The idea

An arena. Every player owns one bot in it, and the player never controls the
bot directly: their **agent** does. The player's only input is a prompt.

![Client screenshot](screenshot.png)

## Round loop

1. **Prompting** — every player types a prompt for their agent
   ("circle the enemy and only punch when its cooldown is running").
2. Each player's agent turns the prompt into a **Lua script**. That script is
   the bot's whole brain for the round.
3. When every online player's script is ready, a **countdown** starts.
4. **Fight** — all bots run their scripts at the same time. Nobody can
   intervene.
5. **Results** — the winner is shown, then the next round starts at step 1.

## What a bot can do

Kept deliberately small for now. Each simulation tick the script returns one of:

| Action | Lua | Effect |
| --- | --- | --- |
| walk | `{ action = "walk", dx = 1, dy = 0 }` | One step in direction (dx, dy) |
| attack | `{ action = "attack" }` | Punch the nearest enemy in range |
| idle | `{ action = "idle" }` or `nil` | Nothing |

One action per tick: a bot cannot walk and punch at once.

The original sketch also lists **run**. It is not implemented yet; see
"Open questions".

### The script contract

```lua
function tick(state)
  -- state.tick                      current tick, starting at 1
  -- state.me      { id, x, y, hp, cooldown }
  -- state.enemies { { id, x, y, hp, dist }, ... }   living enemies only
  -- state.arena   { half_size }
  -- state.rules   { ticks_per_sec, max_ticks, walk_speed, attack_range,
  --                 attack_damage, attack_cooldown, max_hp }
  return { action = "walk", dx = 1, dy = 0 }
end
```

Globals persist between ticks, so a script can remember things.
`sim/scripts/chaser.lua` is the reference bot.

### Rules (all in `sim/src/rules.rs`)

- Square arena, 20 × 20 units. Bots are spheres of radius 0.5, spawn on a
  circle, are solid (cannot overlap) and cannot leave the arena.
- 10 ticks per second, 60 seconds maximum.
- 100 hp, a punch does 10 damage within 1.5 units, then a 5 tick cooldown.
- Walking speed 3 units per second.
- Attacks in the same tick land simultaneously, so a double knockout is
  possible.
- Last bot standing wins. At the time limit the bot with the most hp wins; a
  tie is a draw.
- A script that errors or runs out of its per-tick instruction budget makes the
  bot idle for that tick. The first error per bot is shown to everyone.

## Screen layout

From the original sketch: the arena seen from a tilted camera with a sphere per
bot, a panel on the right, and a long prompt bar along the bottom.

- **Arena**: bots with name and health bar, big countdown / result text.
- **Right panel**: round and phase, players and whether their agent is done,
  the Lua your own agent wrote, script errors.
- **Bottom bar**: name entry, then the prompt box.

## Architecture

```
            prompt                    POST /job
 client  ───────────►  SpacetimeDB  ───────────►  agent worker ──► model API
 (Bevy)  ◄───────────    module     ◄─────────── (Cloudflare)
          game state,   (server/)    deliver_script
          match frames  validates    (HTTP reducer call)
```

| Crate | What it is |
| --- | --- |
| `sim/` | The rules, the sandboxed Lua runtime and the match loop. Pure Rust, no I/O. |
| `server/` | SpacetimeDB module: tables, reducers, phase timers. Links `sim`. |
| `bindings/` | Generated client bindings for the module (`make bindings`). |
| `client/` | Bevy client, desktop and browser. |
| `worker/` | The agent: a Cloudflare Worker (TypeScript) that turns a prompt into Lua with a model (Claude or any OpenAI-format API). |

Decisions worth knowing about:

- **The server simulates the whole match in one go.** When the last script
  arrives, a reducer runs the match to the end and stores one `frame` row per
  tick. Clients just replay the frames. This keeps the server authoritative,
  keeps Lua out of the browser, and makes matches deterministic for a given
  set of scripts and seed.
- **Lua runs in [piccolo](https://github.com/kyren/piccolo)**, a pure-Rust Lua
  VM, because the module is WebAssembly and cannot link C Lua. Scripts get
  `math`, `string`, `table` and the base library only, a fuel budget per tick
  and a memory cap.
- **The agent is a Cloudflare Worker, not part of the module.** A SpacetimeDB
  module can only make HTTP calls that finish within 500 ms, far too short for
  a model to write code. So the module only *pokes* the Worker (`POST /job`,
  answered at once) and the Worker calls the `deliver_script` reducer over
  SpacetimeDB's HTTP API when the model is done. Both directions carry
  `AGENT_SECRET`. A Worker costs nothing while idle and waiting on the model
  is not billed, which is why it is cheap.
- **The module is the only judge of a script.** The Worker does no checking
  and keeps no state. `deliver_script` validates the Lua with the sim. A valid
  script makes the player ready. An invalid one is sent back to the Worker
  with the error for one more try; until a script passes, the player just
  stays "generating". After the last failed try, or 45 s without an answer,
  the submission fails and the player can prompt again.
- **Small, fast models by default** (`claude-haiku-5-5`, `gpt-6-luna`). A bot
  script is a small task and everyone is waiting on the slowest agent.
- **Private by default.** Prompts and scripts are in private tables; a player
  sees only their own script through a view.
- **A house bot** (the reference chaser) joins when a player is alone, so the
  loop can be played solo.

## Open questions

- **Run**: the sketch lists it. Needs a cost (stamina? cannot attack while
  running?) or it simply replaces walk.
- **Prompt deadline**: a round currently waits for every online player. One
  idle player stalls the room. Add a timer and a default bot?
- **Peeking**: match frames are public the moment the match is simulated,
  i.e. during the countdown. A modified client could read the result early.
  Fine while it is friends playing; fix by revealing frames on a timer.
- **Rooms**: there is a single global game. Lobbies and matchmaking come later.
- **Identity**: the browser keeps your player for the tab session; the desktop
  client is a new player on every launch.
- **Agent time limit**: the Worker finishes in the background after answering
  the module, where Cloudflare allows about 30 s. Fine for a small model; a
  slow one would need a Cloudflare Queue.
- **Who pays for the agent?** One shared API key on the worker today. Options:
  bring your own key, or per-player budgets.
- **Agent depth**: today the agent is a single model call. It could become a
  real agent that tests its bot against the house bot before delivering.
- **More to do in a fight**: facing, ranged attacks, obstacles, pickups, and
  seeing what the enemy did last tick.
