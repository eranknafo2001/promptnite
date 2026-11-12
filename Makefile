# Local development. Each long-running target wants its own terminal:
#
#   make server     local SpacetimeDB
#   make publish    build + upload the module (again after changing server/ or sim/)
#   make client     desktop client      |  make web   browser client
#   make worker     the agent (Cloudflare Worker) on http://127.0.0.1:8787
#
# Without AGENT_URL the module hands every prompt the example bot. To use the
# real agent, publish with the Worker's public URL (see README):
#
#   make publish AGENT_URL=https://promptnite-agent.<you>.workers.dev
#
# AGENT_SECRET is the shared password between the module and the agent worker.
#
# Logs stay local unless told otherwise (see "Logging" in the README):
#
#   make publish AXIOM_TOKEN=xaat-...                     the module's logs
#   make client  LOG_URL=http://127.0.0.1:8787/logs       the desktop client's

DB           ?= promptnite
STDB_ADDR    ?= 127.0.0.1:3210
AGENT_SECRET ?= dev-secret
AGENT_URL    ?=
AXIOM_TOKEN  ?=
LOG_URL      ?=

.PHONY: server publish bindings worker worker-deploy client web web-release test

server:
	spacetime start --listen-addr $(STDB_ADDR)

publish:
	AGENT_URL=$(AGENT_URL) AGENT_SECRET=$(AGENT_SECRET) AXIOM_TOKEN=$(AXIOM_TOKEN) spacetime publish $(DB) --yes

# Regenerate bindings/ after changing tables or reducers in server/. The
# generator formats in its own style; `cargo fmt` brings it back to ours, which
# is what is committed and what CI compares against.
bindings:
	spacetime generate --lang rust --out-dir bindings/src/module_bindings --module-path server --yes
	cargo fmt -p promptnite-bindings

# Needs worker/.dev.vars (copy worker/.dev.vars.example) and `npm install`.
worker:
	cd worker && npm run dev

worker-deploy:
	cd worker && npm run deploy

client:
	PROMPTNITE_URI=http://$(STDB_ADDR) PROMPTNITE_DB=$(DB) PROMPTNITE_LOG_URL=$(LOG_URL) cargo run -p promptnite

# Needs `cargo install --locked trunk`. Serves http://127.0.0.1:8080
web:
	cd client && trunk serve

# Relative paths, so client/dist also works from a subfolder (itch.io).
web-release:
	cd client && trunk build --release --public-url ./

test:
	cargo test
