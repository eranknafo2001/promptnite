<img src="assets/icon.svg" width="64" height="64" alt="">

# promptnite

An arena game where you don't control your bot — your agent does. Each round
you write a prompt, your agent writes the bot's Lua, and the bots fight it out.

Built with [Bevy](https://bevy.org) and [SpacetimeDB](https://spacetimedb.com).
The idea, rules and architecture are in [docs/GAME_DESIGN.md](docs/GAME_DESIGN.md).

## Setup

- Rust (see `rust-version` in `Cargo.toml`) with the wasm target:
  `rustup target add wasm32-unknown-unknown`
- [SpacetimeDB CLI](https://spacetimedb.com/install) 2.15
- For the browser build: `cargo install --locked trunk`
- For the agent: Node.js, then `cd worker && npm install`

## Run

Each in its own terminal:

```sh
make server    # local SpacetimeDB on 127.0.0.1:3210
make publish   # build and upload the module (repeat after changing server/ or sim/)
make client    # desktop client
make web       # browser client on http://127.0.0.1:8080
```

When the client opens, enter a name and a room id. Everyone who enters the
same room id plays together, up to four players; whoever got there first owns the room and sets
the match length and whether the house bot fights, and can put teleporters on the roof: a bot
that gets onto one comes out of the next. Alone, you fight the house
bot. For a second player open another browser tab or start another desktop
client and enter the same room id.

Published like this, there is no agent: every prompt gets the built-in example
bot, which is enough to try the game loop.

## The agent

Real bots are written by a model, called from a small
[Cloudflare Worker](https://developers.cloudflare.com/workers/) in `worker/`.
The module sends it each prompt; it sends back whatever Lua the model wrote.
The module checks the script and only then marks the player ready.

Deploy it:

```sh
cd worker
npx wrangler secret put AGENT_SECRET        # any password you make up
npx wrangler secret put ANTHROPIC_API_KEY   # or OPENAI_API_KEY
npx wrangler deploy --var SPACETIMEDB_URI:https://<your spacetimedb host> --var SPACETIMEDB_DB:promptnite
```

Then publish the module with the Worker's URL and the same secret:

```sh
make publish AGENT_URL=https://promptnite-agent.<you>.workers.dev AGENT_SECRET=<that password>
```

| Worker setting | |
| --- | --- |
| `ANTHROPIC_API_KEY` | Use Claude (default model `claude-haiku-5-5`) |
| `OPENAI_API_KEY` | Use OpenAI (default model `gpt-6-luna`) |
| `PROMPTNITE_PROVIDER` | `anthropic` or `openai`, when both keys are set |
| `PROMPTNITE_MODEL` | Any other model of that provider |
| `OPENAI_BASE_URL` | Another service that speaks the OpenAI Chat Completions format |

**Running the agent locally.** `make worker` starts it on
`http://127.0.0.1:8787` (copy `worker/.dev.vars.example` to `worker/.dev.vars`
first). SpacetimeDB refuses to send requests to local addresses, so a local
module can't reach it there. Give it a public address with a tunnel, for
example `cloudflared tunnel --url http://127.0.0.1:8787`, and publish with
`AGENT_URL` set to the tunnel's URL.

## Logging

The module, the worker and the client can send their logs to one
[Axiom](https://axiom.co) dataset, so a prompt can be followed from the
player's screen through the module to the model and back. It is off until
there is a token:

1. Create an Axiom account (the free plan is plenty) and a dataset named
   `promptnite` in the **US East** region. The hosted SpacetimeDB is in
   Virginia and gives the module half a second per request, which another
   region may not make.
2. Create an API token (Settings → API tokens) that may only ingest into that
   dataset.
3. Give it to the worker and the module as `AXIOM_TOKEN`: the repository
   secret below for deploys, or by hand with
   `npx wrangler secret put AXIOM_TOKEN` and `make publish AXIOM_TOKEN=…`.

| Who | How its logs get there |
| --- | --- |
| Module | `emit(...)` in `server/src/lib.rs` stores the event; a scheduled procedure sends them a couple of seconds later |
| Worker | `Logger` in `worker/src/log.ts`, one batch per request |
| Client | Warnings, errors, panics and its own `info!` go to the worker's `POST /logs`, which passes them on. The token never reaches the client |

Every event has `source` (`module`, `worker` or `client`), `level` and
`message`, plus the ids that apply: `job_id`, `room_id`, `player_id`,
`identity`. In Axiom:

```
['promptnite'] | where job_id == 42 | sort by _time asc
['promptnite'] | where level == "error" | summarize count() by source, message
```

Things to know:

- A reducer that fails takes its log events with it, because they are stored
  in the same transaction. Those still show in `spacetime logs promptnite`.
- Prompts are not logged, only their length.
- The client is told where the worker is by the deploy. Elsewhere, use
  `?log=<worker>/logs` in the browser or `PROMPTNITE_LOG_URL` on desktop
  (`make client LOG_URL=http://127.0.0.1:8787/logs`).
- To turn logging off again, remove the repository secret and also run
  `npx wrangler secret delete AXIOM_TOKEN` in `worker/`: a deploy leaves the
  worker's existing secrets in place.
- A module on your own machine may be too far from Axiom to send in time. Its
  events wait and are retried; `spacetime logs` has them regardless.

## CI and deploys

Pull requests run [CI](.github/workflows/ci.yml): formatting, clippy and tests,
the module build (and that `bindings/` matches it), the browser build, and the
worker's typecheck.

Every push to `main` runs the same checks and then
[deploys](.github/workflows/deploy.yml): the agent to Cloudflare Workers, the
module to SpacetimeDB, and the browser client to [itch.io](https://itch.io),
built with the hosted database as its default. A module change that would lose
data is not published; do that one by hand.

The itch.io game page has to exist before the first deploy. After that first
upload, edit the page once: set the kind of project to HTML and tick "This
file will be played in the browser" on the uploaded file.

Repository secrets (Settings → Secrets and variables → Actions):

| Secret | |
| --- | --- |
| `SPACETIMEDB_TOKEN` | Who publishes the module. `spacetime login`, then `spacetime login show --token` |
| `CLOUDFLARE_API_TOKEN` | A Cloudflare API token that can edit Workers Scripts |
| `CLOUDFLARE_ACCOUNT_ID` | The Cloudflare account to deploy to |
| `AGENT_SECRET` | Any password you make up; given to both the module and the worker |
| `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | The model that writes the bots |
| `BUTLER_API_KEY` | An itch.io API key, from itch.io → Settings → API keys |
| `AXIOM_TOKEN` | Optional. Turns on [logging](#logging) |

Repository variables. Only `ITCH_TARGET` is required; the rest show their defaults:

| Variable | |
| --- | --- |
| `ITCH_TARGET` | The itch.io game to upload to, as `user/game` |
| `ITCH_CHANNEL` | `html5`, the itch.io channel the build is pushed to |
| `SPACETIMEDB_URI` | `https://maincloud.spacetimedb.com` |
| `SPACETIMEDB_DB` | `promptnite`. Database names are shared by everyone on the host, so pick another if it is taken |
| `AGENT_URL` | The worker's `workers.dev` address. Set it if the worker has a custom domain |
| `PROMPTNITE_PROVIDER`, `PROMPTNITE_MODEL`, `OPENAI_BASE_URL` | Unset, see the table above |
| `AXIOM_DATASET` | `promptnite` |
| `AXIOM_DOMAIN` | `us-east-1.aws.edge.axiom.co`. For a dataset in the EU, `eu-central-1.aws.edge.axiom.co` |

## Layout

| Path | |
| --- | --- |
| `sim/` | Rules, sandboxed Lua, match simulation |
| `server/` | SpacetimeDB module |
| `bindings/` | Generated module bindings (`make bindings`) |
| `client/` | Bevy client, desktop and browser |
| `worker/` | Agent: prompt → Lua (Cloudflare Worker, TypeScript; Claude or OpenAI-format APIs) |

The browser client connects to `http://127.0.0.1:3210` / `promptnite` by
default; override with `?uri=…&db=…`. The desktop client reads `PROMPTNITE_URI`
and `PROMPTNITE_DB`.

Created in [T3 Code](https://t3.codes).

### Bot loadouts

Generated bots allocate up to 10 points across `hp`, `speed`, `damage`,
`range`, and `cooldown` (integer 0–5 each):

```lua
build = { hp = 1, speed = 4, damage = 1, range = 3, cooldown = 1 }
```

Build initialization uses a fixed seed so the preview matches combat; tick-time
randomness uses the match seed. Stats are fixed when the script loads. HP is `60 + 20*hp`, speed is
`2.2 + 0.4*speed`, damage is `6 + 2*damage`, reach is `1.2 + 0.15*range`,
and cooldown is `7 - cooldown` ticks. Missing builds retain legacy stats;
invalid declarations are rejected by script validation. Both `state.me` and
enemies expose the resulting stats; `state.rules` reflects the acting bot.
