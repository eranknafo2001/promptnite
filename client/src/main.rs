//! Promptnite game client.
//!
//! The client is a viewer plus a text box: it shows the arena, lets the player
//! send a prompt, and replays the match the server simulated. It runs no game
//! logic and no Lua, which keeps the browser build simple.
//!
//!   net.rs    connection to SpacetimeDB, subscriptions, identity token
//!   arena.rs  3D scene and match replay
//!   fx.rs     explosions, debris and camera shake
//!   ui.rs     egui overlay: name screen, prompt box, status lines
//!   clipboard.rs  copy, cut and paste in the text boxes
//!   theme.rs  colours, type sizes and boxes: the look in one place
//!   sandbox.rs  every screen with made-up data and no server (`--sandbox`)
//!   logs.rs   sends warnings, errors and panics to the central log

mod arena;
mod clipboard;
mod fx;
mod logs;
mod net;
mod sandbox;
mod theme;
mod ui;

use bevy::{asset::AssetMetaCheck, log::LogPlugin, prelude::*};

fn main() {
    let sandbox = sandbox::enabled();
    let mut app = App::new();
    app.insert_resource(ClearColor(theme::BACKGROUND))
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: if sandbox {
                            "promptnite sandbox"
                        } else {
                            "promptnite"
                        }
                        .into(),
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
                })
                // The assets ship without `.meta` files, so never ask for one.
                // Asking costs a request per asset in the browser, and a server
                // that answers a missing file with a page instead of a 404
                // (`trunk serve` does) makes Bevy reject the model.
                .set(AssetPlugin {
                    meta_check: AssetMetaCheck::Never,
                    ..default()
                }),
        )
        .add_plugins((
            arena::plugin,
            fx::plugin,
            ui::plugin,
            clipboard::plugin,
            logs::plugin,
        ));
    if sandbox {
        app.add_plugins(sandbox::plugin);
    } else {
        app.add_plugins(net::plugin);
    }
    app.run();
}
