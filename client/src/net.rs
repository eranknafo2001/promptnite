//! Connection to the SpacetimeDB module.
//!
//! The rest of the client doesn't react to row events. It simply reads the
//! SDK's local cache (`conn.db()`) every frame, which is plenty for a game
//! with a handful of rows and makes reconnects a non-issue.
//!
//! The module's tables are private. What we subscribe to are its views, which
//! answer for whoever is asking: our own player row, and the rows of the room
//! the server has put us in. They fill in by themselves once we join
//! ([`Session`]), and a match's frames arrive when its fight starts.

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy_stdb::prelude::*;
use promptnite_bindings::{sdk::Table, *};

use crate::ui::{Action, Screen, Snapshot};

pub type Conn = StdbConnection<DbConnection>;
type Subs = StdbSubscriptions<SubKey, RemoteModule>;
type Cmds<'w, 's> = StdbCommands<'w, 's, DbConnection, RemoteModule>;

// Where to connect when nothing is given at launch: local development, unless
// the build baked in its own (the deploy workflow does, for the hosted client).
const DEFAULT_URI: &str = match option_env!("PROMPTNITE_DEFAULT_URI") {
    Some(uri) => uri,
    None => "http://127.0.0.1:3210",
};
const DEFAULT_DATABASE: &str = match option_env!("PROMPTNITE_DEFAULT_DB") {
    Some(database) => database,
    None => "promptnite",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SubKey {
    Me,
    Room,
    Players,
    Submissions,
    Frames,
    Faults,
    Teleporters,
}

/// Where we are connecting to.
#[derive(Resource)]
struct Server {
    uri: String,
    database: String,
}

/// The name and room the player chose at this launch.
#[derive(Resource, Default)]
pub struct Session {
    joined: Option<(String, String)>,
    /// The server hasn't been told yet (or not since the last reconnect).
    unsent: bool,
    /// Why the server turned the last join down (the room is full, ...).
    error: Option<String>,
    /// Filled in by the `join` reducer's callback, which runs outside Bevy's
    /// systems; `send_join` moves it into `error`.
    refusal: Arc<Mutex<Option<String>>>,
    /// Why the server would not take our last prompt (sent too soon after the
    /// one before, the agents are busy, ...). Written by the `submit_prompt`
    /// reducer's callback, read into every [`Snapshot`].
    prompt_refusal: Arc<Mutex<Option<String>>>,
}

impl Session {
    fn join(&mut self, name: String, room: String) {
        self.joined = Some((name, room));
        self.unsent = true;
        self.error = None;
    }
}

/// Our own player row, if we ever played on this identity.
pub fn me(conn: &Conn) -> Option<Player> {
    let identity = conn.try_identity()?;
    // After signing in as somebody else the cache may hold the old row for a
    // moment.
    conn.db()
        .my_player()
        .iter()
        .find(|player| player.identity == identity)
}

/// Us and the room we are in, once the server has let us in.
fn my_room(conn: &Conn) -> Option<(Player, Room)> {
    // `online` is only set by joining, so a row left over from an earlier
    // launch doesn't count.
    let me = me(conn).filter(|me| me.online)?;
    let room = conn
        .db()
        .my_room()
        .iter()
        .find(|room| room.id == me.room_id)?;
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
    .insert_resource(Screen::Connecting(server.uri.clone()))
    .insert_resource(server)
    .init_resource::<Session>()
    .add_systems(Startup, connect)
    .add_systems(
        Update,
        (on_connected, perform, send_join, publish_screen)
            .chain()
            .before(crate::arena::replay),
    );
}

/// Everything the UI needs, read from the SDK cache once per frame.
fn publish_screen(
    conn: Option<Res<Conn>>,
    server: Res<Server>,
    session: Res<Session>,
    mut screen: ResMut<Screen>,
) {
    *screen = match conn.as_deref().filter(|conn| conn.is_active()) {
        None => Screen::Connecting(server.uri.clone()),
        Some(conn) => match snapshot(conn, &session) {
            Some(snapshot) => Screen::Room(Box::new(snapshot)),
            None if session.joined.is_some() => Screen::Entering,
            None => Screen::Join {
                known_name: me(conn).map(|me| me.name),
                error: session.error.clone(),
            },
        },
    };
}

/// `None` until the player is in a room.
fn snapshot(conn: &Conn, session: &Session) -> Option<Snapshot> {
    let db = conn.db();
    let (me, game) = my_room(conn)?;
    // The cache can briefly hold rows of a room we just left.
    let mut players: Vec<Player> = db
        .room_players()
        .iter()
        .filter(|player| player.room_id == game.id)
        .collect();
    players.sort_by_key(|player| player.id);
    let mut teleporters: Vec<Teleporter> = db
        .room_teleporters()
        .iter()
        .filter(|pad| pad.room_id == game.id)
        .collect();
    teleporters.sort_by_key(|pad| pad.number);
    Some(Snapshot {
        submissions: db
            .room_submissions()
            .iter()
            .filter(|s| s.room_id == game.id && s.round == game.round)
            .collect(),
        faults: db
            .match_faults()
            .iter()
            .filter(|fault| fault.room_id == game.id)
            .collect(),
        prompt_refusal: session.prompt_refusal.lock().unwrap().clone(),
        game,
        players,
        teleporters,
        me,
    })
}

/// Carry out what the player asked for in the UI.
fn perform(
    mut actions: MessageReader<Action>,
    conn: Option<Res<Conn>>,
    mut session: ResMut<Session>,
) {
    for action in actions.read() {
        let result = match (action.clone(), conn.as_deref()) {
            (Action::Join { name, room }, _) => {
                session.join(name, room);
                Ok(())
            }
            (Action::SubmitPrompt(prompt), Some(conn)) => {
                let refusal = session.prompt_refusal.clone();
                conn.reducers()
                    .submit_prompt_then(prompt, move |_, result| {
                        *refusal.lock().unwrap() = match result {
                            Ok(Err(error)) => Some(error),
                            _ => None,
                        };
                    })
            }
            (
                Action::SetRoomSettings {
                    match_secs,
                    house_bot,
                },
                Some(conn),
            ) => conn.reducers().set_room_settings(match_secs, house_bot),
            (Action::NextRound, Some(conn)) => conn.reducers().next_round(),
            (Action::AddTeleporter { x, y }, Some(conn)) => conn.reducers().add_teleporter(x, y),
            (Action::RemoveTeleporter(number), Some(conn)) => {
                conn.reducers().remove_teleporter(number)
            }
            (_, None) => Ok(()),
        };
        if let Err(err) = result {
            warn!("could not call reducer: {err}");
        }
    }
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
    // Keep the first token. When we connect with one, the browser SDK trades
    // it for a token that expires in a minute and the server hands that one
    // back here: saving it would lock this tab out on its next reload.
    if token::load().is_none() {
        token::save(&message.access_token);
    }
    // The server forgets that we are in the room when the connection drops.
    session.unsent = session.joined.is_some();

    // The views know who is asking, so there is nothing to filter by.
    subs.subscribe_query(SubKey::Me, |q| q.from.my_player());
    subs.subscribe_query(SubKey::Room, |q| q.from.my_room());
    subs.subscribe_query(SubKey::Players, |q| q.from.room_players());
    subs.subscribe_query(SubKey::Submissions, |q| q.from.room_submissions());
    subs.subscribe_query(SubKey::Frames, |q| q.from.match_frames());
    subs.subscribe_query(SubKey::Faults, |q| q.from.match_faults());
    subs.subscribe_query(SubKey::Teleporters, |q| q.from.room_teleporters());
}

fn send_join(conn: Option<Res<Conn>>, mut session: ResMut<Session>) {
    let refused = session.refusal.lock().unwrap().take();
    if let Some(error) = refused {
        // Back to the join form, with the reason.
        session.joined = None;
        session.unsent = false;
        session.error = Some(error);
        return;
    }
    if !session.unsent {
        return;
    }
    let Some(conn) = conn.filter(|conn| conn.is_active()) else {
        return;
    };
    if let Some((name, room)) = session.joined.clone() {
        let refusal = session.refusal.clone();
        let sent = conn.reducers().join_then(name, room, move |_, result| {
            if let Ok(Err(error)) = result {
                *refusal.lock().unwrap() = Some(error);
            }
        });
        match sent {
            Ok(()) => session.unsent = false,
            Err(err) => warn!("could not join: {err}"),
        }
    }
}

/// A setting from `?key=value` in the page URL (browser) or an environment
/// variable (desktop).
pub fn setting(_query_key: &str, _env_key: &str) -> Option<String> {
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
            storage()?.get_item("promptnite_identity").ok()?
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            None
        }
    }

    pub fn save(_token: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let _ = storage.set_item("promptnite_identity", _token);
        }
    }
}
