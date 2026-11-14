//! Promptnite simulation core.
//!
//! Everything that decides *what happens in the arena* lives here and nowhere
//! else: the rules ([`rules`]), the sandboxed Lua runner that turns a bot script
//! into actions ([`lua`]) and the match loop that ties them together
//! ([`run_match`]).
//!
//! The crate is pure Rust with no I/O, clock or platform dependencies, so the
//! exact same code runs natively (unit tests), inside the SpacetimeDB module
//! (wasm, authoritative) and could run in the browser for a local sandbox.

pub mod lua;
pub mod rules;

mod sim;

pub use lua::{LuaBot, validate_script};
pub use sim::{
    Action, ActionKind, BotSnapshot, BotSpec, Fault, Frame, MatchOutcome, Teleporter, run_match,
};

/// A script that always works. The server uses it when no LLM is configured,
/// and it doubles as the reference example we show the agent.
pub const EXAMPLE_SCRIPT: &str = include_str!("../scripts/chaser.lua");
