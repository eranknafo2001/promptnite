//! Promptnite game client.
//!
//! The client is a viewer plus a text box: it shows the arena, lets the player
//! send a prompt, and replays the match the server simulated. It runs no game
//! logic and no Lua, which keeps the browser build simple.
//!
//!   net.rs    connection to SpacetimeDB, subscriptions, identity token
//!   arena.rs  3D scene and match replay
//!   ui.rs     egui overlay: name screen, prompt box, status lines
//!   theme.rs  colours, type sizes and boxes: the look in one place
//!   sandbox.rs  every screen with made-up data and no server (`--sandbox`)

mod arena;
mod net;
mod sandbox;
mod theme;
mod ui;

use bevy::prelude::*;

fn main() {
    let sandbox = sandbox::enabled();
    let mut app = App::new();
    app.insert_resource(ClearColor(theme::BACKGROUND))
        .add_plugins(
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: if sandbox {
                        "promptnite sandbox"
                    } else {
                        "promptnite"
                    }
                    .into(),
                    // Browser: render into the canvas from index.html and follow
                    // its size. Both settings are ignored on desktop.
                    canvas: Some("#game".into()),
                    fit_canvas_to_parent: true,
                    ..default()
                }),
                ..default()
            }),
        )
        .add_plugins((arena::plugin, ui::plugin));
    if sandbox {
        app.add_plugins(sandbox::plugin);
    } else {
        app.add_plugins(net::plugin);
    }
    app.run();
}
