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
same room id plays together; whoever got there first owns the room and sets
the match length and whether the house bot fights. Alone, you fight the house
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

## CI and deploys

Pull requests run [CI](.github/workflows/ci.yml): formatting, clippy and tests,
the module build (and that `bindings/` matches it), the browser build, and the
worker's typecheck.

Every push to `main` runs the same checks and then
[deploys](.github/workflows/deploy.yml): the agent to Cloudflare Workers and
the module to SpacetimeDB. A module change that would lose data is not
published; do that one by hand.

The browser client is not deployed. Each run on `main` builds it with the
hosted database as its default and keeps it as the `web-dist` artifact; that
zip has `index.html` at its top level, which is what itch.io wants for an
HTML5 game.

Repository secrets (Settings → Secrets and variables → Actions):

| Secret | |
| --- | --- |
| `SPACETIMEDB_TOKEN` | Who publishes the module. `spacetime login`, then `spacetime login show --token` |
| `CLOUDFLARE_API_TOKEN` | A Cloudflare API token that can edit Workers Scripts |
| `CLOUDFLARE_ACCOUNT_ID` | The Cloudflare account to deploy to |
| `AGENT_SECRET` | Any password you make up; given to both the module and the worker |
| `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | The model that writes the bots |

Optional repository variables, with their defaults:

| Variable | |
| --- | --- |
| `SPACETIMEDB_URI` | `https://maincloud.spacetimedb.com` |
| `SPACETIMEDB_DB` | `promptnite`. Database names are shared by everyone on the host, so pick another if it is taken |
| `AGENT_URL` | The worker's `workers.dev` address. Set it if the worker has a custom domain |
| `PROMPTNITE_PROVIDER`, `PROMPTNITE_MODEL`, `OPENAI_BASE_URL` | Unset, see the table above |

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
