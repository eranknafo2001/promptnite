//! Connection to the SpacetimeDB module.
//!
//! The rest of the client doesn't react to row events. It simply reads the
//! SDK's local cache (`conn.db()`) every frame, which is plenty for a game
//! with a handful of rows and makes reconnects a non-issue.
//!
//! A player picks a room every time the app starts ([`Session`]). Until then
//! we only subscribe to our own player row; once the server has put us in a
//! room we subscribe to that room's rows and nothing else.

use bevy::prelude::*;
use bevy_stdb::prelude::*;
use promptnite_bindings::{sdk::Identity, *};

pub type Conn = StdbConnection<DbConnection>;
type Subs = StdbSubscriptions<SubKey, RemoteModule>;
type Cmds<'w, 's> = StdbCommands<'w, 's, DbConnection, RemoteModule>;

const DEFAULT_URI: &str = "http://127.0.0.1:3210";
const DEFAULT_DATABASE: &str = "promptnite";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SubKey {
    Me,
    Room,
    Players,
    Submissions,
    Frames,
    Faults,
}

/// Where we are connecting to, for display.
#[derive(Resource)]
pub struct Server {
    pub uri: String,
    pub database: String,
}

/// The name and room the player chose at this launch.
#[derive(Resource, Default)]
pub struct Session {
    joined: Option<(String, String)>,
    /// The server hasn't been told yet (or not since the last reconnect).
    unsent: bool,
}

impl Session {
    pub fn join(&mut self, name: String, room: String) {
        self.joined = Some((name, room));
        self.unsent = true;
    }

    pub fn has_joined(&self) -> bool {
        self.joined.is_some()
    }
}

/// Our own player row, if we ever played on this identity.
pub fn me(conn: &Conn) -> Option<Player> {
    conn.db().player().identity().find(&conn.try_identity()?)
}

/// Us and the room we are in, once the server has let us in.
pub fn my_room(conn: &Conn) -> Option<(Player, Room)> {
    // `online` is only set by joining, so a row left over from an earlier
    // launch doesn't count.
    let me = me(conn).filter(|me| me.online)?;
    let room = conn.db().room().id().find(&me.room_id)?;
    Some((me, room))
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
    .init_resource::<Session>()
    .add_systems(Startup, connect)
    .add_systems(Update, (on_connected, send_join, follow_room).chain());
}

fn connect(mut cmds: Cmds) {
    cmds.connect(match token::load() {
        Some(token) => StdbConnectOptions::from_token(token),
        None => StdbConnectOptions::default(),
    });
}

fn on_connected(
    mut connected: ReadStdbConnectedMessage,
    mut subs: ResMut<Subs>,
    mut session: ResMut<Session>,
) {
    let Some(message) = connected.read().last() else {
        return;
    };
    info!("connected as {}", message.identity);
    token::save(&message.access_token);
    // The server forgets that we are in the room when the connection drops.
    session.unsent = session.joined.is_some();

    let identity: Identity = message.identity;
    subs.subscribe_query(SubKey::Me, |q| {
        q.from
            .player()
            .r#where(|player| player.identity.eq(identity))
    });
}

fn send_join(conn: Option<Res<Conn>>, mut session: ResMut<Session>) {
    if !session.unsent {
        return;
    }
    let Some(conn) = conn.filter(|conn| conn.is_active()) else {
        return;
    };
    if let Some((name, room)) = session.joined.clone() {
        match conn.reducers().join(name, room) {
            Ok(()) => session.unsent = false,
            Err(err) => warn!("could not join: {err}"),
        }
    }
}

/// Subscribe to the rows of whichever room we are in.
fn follow_room(conn: Option<Res<Conn>>, mut subs: ResMut<Subs>, mut following: Local<u32>) {
    // Not `my_room`: the room's row only arrives through these subscriptions.
    let Some(me) = conn.as_deref().and_then(me).filter(|me| me.online) else {
        return;
    };
    let id = me.room_id;
    if *following == id {
        return;
    }
    *following = id;
    // Subscribing again under the same key replaces the previous room's query.
    subs.subscribe_query(SubKey::Room, |q| q.from.room().r#where(|c| c.id.eq(id)));
    subs.subscribe_query(SubKey::Players, |q| {
        q.from.player().r#where(|c| c.room_id.eq(id))
    });
    subs.subscribe_query(SubKey::Submissions, |q| {
        q.from.submission().r#where(|c| c.room_id.eq(id))
    });
    subs.subscribe_query(SubKey::Frames, |q| {
        q.from.frame().r#where(|c| c.room_id.eq(id))
    });
    subs.subscribe_query(SubKey::Faults, |q| {
        q.from.match_fault().r#where(|c| c.room_id.eq(id))
    });
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
