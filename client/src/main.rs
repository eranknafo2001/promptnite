//! Promptnite game client.
//!
//! The client is a viewer plus a text box: it shows the arena, lets the player
//! send a prompt, and replays the match the server simulated. It runs no game
//! logic and no Lua, which keeps the browser build simple.
//!
//!   net.rs    connection to SpacetimeDB, subscriptions, identity token
//!   arena.rs  3D scene and match replay
//!   ui.rs     egui overlay: name screen, prompt box, status lines
//!   logs.rs   sends warnings, errors and panics to the central log

mod arena;
mod logs;
mod net;
mod ui;

use bevy::{log::LogPlugin, prelude::*};

fn main() {
    App::new()
        .insert_resource(ClearColor(Color::srgb(0.07, 0.08, 0.10)))
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "promptnite".into(),
                        // Browser: render into the canvas from index.html and
                        // follow its size. Both settings are ignored on desktop.
                        canvas: Some("#game".into()),
                        fit_canvas_to_parent: true,
                        ..default()
                    }),
                    ..default()
                })
                .set(LogPlugin {
                    custom_layer: logs::layer,
                    ..default()
                }),
        )
        .add_plugins((net::plugin, arena::plugin, ui::plugin, logs::plugin))
        .run();
}
