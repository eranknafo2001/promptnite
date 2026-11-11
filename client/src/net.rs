//! Connection to the SpacetimeDB module.
//!
//! The rest of the client doesn't react to row events. It simply reads the
//! SDK's local cache (`conn.db()`) every frame, which is plenty for a game
//! with a handful of rows and makes reconnects a non-issue.

use bevy::prelude::*;
use bevy_stdb::prelude::*;
use promptnite_bindings::*;

pub type Conn = StdbConnection<DbConnection>;
type Subs = StdbSubscriptions<SubKey, RemoteModule>;
type Cmds<'w, 's> = StdbCommands<'w, 's, DbConnection, RemoteModule>;

const DEFAULT_URI: &str = "http://127.0.0.1:3210";
const DEFAULT_DATABASE: &str = "promptnite";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SubKey {
    Game,
    Players,
    Submissions,
    Frames,
    Faults,
    MyScript,
}

/// Where we are connecting to, for display.
#[derive(Resource)]
pub struct Server {
    pub uri: String,
    pub database: String,
}

pub fn plugin(app: &mut App) {
    let server = Server {
        uri: setting("uri", "PROMPTNITE_URI").unwrap_or_else(|| DEFAULT_URI.into()),
        database: setting("db", "PROMPTNITE_DB").unwrap_or_else(|| DEFAULT_DATABASE.into()),
    };

    // Native: the SDK runs its own thread. Browser: there are no threads, the
    // SDK schedules a task on the page's event loop instead.
    #[cfg(not(target_arch = "wasm32"))]
    let driver = DbConnection::run_threaded;
    #[cfg(target_arch = "wasm32")]
    let driver = DbConnection::run_background_task;

    app.add_plugins(
        StdbPlugin::<DbConnection, RemoteModule>::default()
            .with_uri(server.uri.clone())
            .with_database_name(server.database.clone())
            .with_subscriptions::<SubKey>()
            .with_reconnect(StdbReconnectOptions::default())
            .with_background_driver(driver),
    )
    .insert_resource(server)
    .add_systems(Startup, connect)
    .add_systems(Update, on_connected);
}

fn connect(mut cmds: Cmds) {
    cmds.connect(match token::load() {
        Some(token) => StdbConnectOptions::from_token(token),
        None => StdbConnectOptions::default(),
    });
}

fn on_connected(mut connected: ReadStdbConnectedMessage, mut subs: ResMut<Subs>) {
    let Some(message) = connected.read().last() else {
        return;
    };
    info!("connected as {}", message.identity);
    token::save(&message.access_token);

    subs.subscribe_query(SubKey::Game, |q| q.from.game());
    subs.subscribe_query(SubKey::Players, |q| q.from.player());
    subs.subscribe_query(SubKey::Submissions, |q| q.from.submission());
    subs.subscribe_query(SubKey::Frames, |q| q.from.frame());
    subs.subscribe_query(SubKey::Faults, |q| q.from.match_fault());
    // A view: the server only ever sends us our own row.
    subs.subscribe_query(SubKey::MyScript, |q| q.from.my_script());
}

/// A setting from `?key=value` in the page URL (browser) or an environment
/// variable (desktop).
fn setting(_query_key: &str, _env_key: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        let search = web_sys::window()?.location().search().ok()?;
        web_sys::UrlSearchParams::new_with_str(&search)
            .ok()?
            .get(_query_key)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var(_env_key).ok()
    }
}

/// The identity token decides which player you are.
///
/// Browser: kept in `sessionStorage`, so a reload keeps your player while a
/// second tab is a second player (handy for testing).
/// Desktop: not stored yet, every launch is a new player.
mod token {
    #[cfg(target_arch = "wasm32")]
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.session_storage().ok()?
    }

    pub fn load() -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            storage()?.get_item("promptnite_token").ok()?
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            None
        }
    }

    pub fn save(_token: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let _ = storage.set_item("promptnite_token", _token);
        }
    }
}
