# Local development. Each long-running target wants its own terminal:
#
#   make server     local SpacetimeDB
#   make publish    build + upload the module (again after changing server/ or sim/)
#   make client     desktop client      |  make web   browser client
#   make worker     the agent (Cloudflare Worker, run locally)
#
# Without AGENT_SECRET the module hands every prompt the example bot. To use
# the real agent, publish with a password and start the worker with the same
# one (worker/.dev.vars, see README):
#
#   make publish AGENT_SECRET=dev-secret
#   make worker

DB           ?= promptnite
STDB_ADDR    ?= 127.0.0.1:3210
AGENT_SECRET ?=
# Only for a deployed worker: where the module wakes it up (see README).
AGENT_URL    ?=

.PHONY: server publish bindings worker worker-deploy client web web-release test

server:
	spacetime start --listen-addr $(STDB_ADDR)

publish:
	AGENT_SECRET=$(AGENT_SECRET) AGENT_URL=$(AGENT_URL) spacetime publish $(DB) --yes

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
	PROMPTNITE_URI=http://$(STDB_ADDR) PROMPTNITE_DB=$(DB) cargo run -p promptnite

# Needs `cargo install --locked trunk`. Serves http://127.0.0.1:8080
web:
	cd client && trunk serve

# Relative paths, so client/dist also works from a subfolder (itch.io).
web-release:
	cd client && trunk build --release --public-url ./

test:
	cargo test
