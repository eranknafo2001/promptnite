//! Sends the client's log events to the agent worker, which passes them on to
//! Axiom next to the module's and its own (see worker/src/log.ts).
//!
//! Nothing changes for the rest of the client: `info!` / `warn!` / `error!`
//! are picked up by a layer in Bevy's logger and posted in batches. Warnings
//! and errors are sent from anywhere, `info!` only from our own code. A panic
//! is sent on the spot, together with whatever was still waiting.
//!
//! This is off unless the client is told where the worker is:
//!
//!   browser  `<meta name="promptnite-log-url">` in index.html (the deploy
//!            workflow fills it in), or `?log=<url>` in the page address
//!   desktop  the `PROMPTNITE_LOG_URL` environment variable
//!
//! The URL is the worker's `/logs` route, e.g. `http://127.0.0.1:8787/logs`.

use std::{
    fmt::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use bevy::{
    log::{
        BoxedLayer, Level,
        tracing::{
            self, Subscriber,
            field::{Field, Visit},
        },
        tracing_subscriber::{Layer, layer::Context},
    },
    prelude::*,
    time::common_conditions::on_real_timer,
};
use serde_json::{Map, Value, json};

use crate::net::{self, Conn};

const SEND_EVERY: Duration = Duration::from_secs(5);
/// The worker takes at most this many events per request.
const MAX_BATCH: usize = 50;
/// Events kept while they can't be sent fast enough; more are dropped.
const MAX_WAITING: usize = 200;
const MAX_MESSAGE_CHARS: usize = 2_000;

/// Which build this is. The CI build sets it to the commit.
const BUILD: &str = match option_env!("PROMPTNITE_BUILD") {
    Some(build) => build,
    None => "dev",
};

struct Entry {
    /// Milliseconds since the Unix epoch.
    time: f64,
    level: &'static str,
    target: String,
    message: String,
    /// How many times in a row this was logged. A warning inside a system
    /// can fire every frame.
    repeats: u32,
}

/// Shared between the logger (any thread), the sending system and the panic
/// hook.
#[derive(Default)]
struct Outbox {
    waiting: Vec<Entry>,
    /// Who we are: session, build and, once known, identity, player and room.
    context: Map<String, Value>,
}

impl Outbox {
    /// The request body for up to `MAX_BATCH` waiting events, if there are any.
    fn take_batch(&mut self) -> Option<String> {
        if self.waiting.is_empty() {
            return None;
        }
        let count = self.waiting.len().min(MAX_BATCH);
        let events: Vec<Value> = self
            .waiting
            .drain(..count)
            .map(|entry| {
                json!({
                    "time": entry.time,
                    "level": entry.level,
                    "target": entry.target,
                    "message": entry.message,
                    "repeats": entry.repeats,
                })
            })
            .collect();
        let mut body = self.context.clone();
        body.insert("events".into(), events.into());
        Some(Value::Object(body).to_string())
    }
}

type Shared = Arc<Mutex<Outbox>>;

#[derive(Resource)]
struct Shipper {
    url: String,
    outbox: Shared,
}

/// Goes into Bevy's `LogPlugin::custom_layer`.
pub fn layer(app: &mut App) -> Option<BoxedLayer> {
    let url = log_url()?;
    let outbox = Shared::default();
    outbox.lock().unwrap().context = Map::from_iter([
        // Tells one launch of the game from another.
        ("session".into(), format!("{:x}", now_ms() as u64).into()),
        ("platform".into(), platform().into()),
        ("build".into(), BUILD.into()),
    ]);

    // After Bevy's own hook, which this one calls when it is done.
    let earlier = std::panic::take_hook();
    let (hook_url, hook_outbox) = (url.clone(), outbox.clone());
    std::panic::set_hook(Box::new(move |panic| {
        // `try_lock`: the panic may have happened while the lock was held.
        if let Ok(mut outbox) = hook_outbox.try_lock() {
            outbox.waiting.push(Entry {
                time: now_ms(),
                level: "error",
                target: "panic".into(),
                message: cut(panic.to_string()),
                repeats: 1,
            });
            // The oldest events go first, so the panic may need a second batch.
            while let Some(body) = outbox.take_batch() {
                send_now(&hook_url, body);
            }
        }
        earlier(panic);
    }));

    app.insert_resource(Shipper {
        url,
        outbox: outbox.clone(),
    });
    Some(Box::new(ShipLayer(outbox)))
}

pub fn plugin(app: &mut App) {
    app.add_systems(
        Update,
        ship.run_if(resource_exists::<Shipper>)
            .run_if(on_real_timer(SEND_EVERY)),
    );
}

fn ship(shipper: Res<Shipper>, conn: Option<Res<Conn>>) {
    let mut outbox = shipper.outbox.lock().unwrap();
    if let Some(conn) = conn.as_deref() {
        if let Some(identity) = conn.try_identity() {
            outbox
                .context
                .insert("identity".into(), identity.to_string().into());
        }
        if let Some(me) = net::me(conn) {
            outbox.context.insert("player_id".into(), me.id.into());
            outbox.context.insert("room_id".into(), me.room_id.into());
        }
    }
    if let Some(body) = outbox.take_batch() {
        // The result is ignored: a warning about it would be sent here too,
        // and fail the same way.
        ehttp::fetch(request(&shipper.url, body), |_| {});
    }
}

/// Plain text, although it is JSON: that way a browser sends it straight away
/// instead of first asking the worker whether a JSON request is allowed (a
/// CORS preflight). The worker parses it either way.
fn request(url: &str, body: String) -> ehttp::Request {
    ehttp::Request::post(url, body.into_bytes())
}

/// Send from a panic hook: nothing runs afterwards, so this can't be left to
/// a background task.
fn send_now(url: &str, body: String) {
    #[cfg(target_arch = "wasm32")]
    if let Some(window) = web_sys::window() {
        let _ = window
            .navigator()
            .send_beacon_with_opt_str(url, Some(&body));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let request = request(url, body).with_timeout(Some(Duration::from_secs(3)));
        let _ = ehttp::fetch_blocking(&request);
    }
}

struct ShipLayer(Shared);

impl<S: Subscriber> Layer<S> for ShipLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = *meta.level();
        if level > Level::INFO {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        // Events from the `log` crate (the SpacetimeDB SDK) all have the
        // target "log" and carry the real one as a field.
        let target = fields.log_target.as_deref().unwrap_or(meta.target());
        if level == Level::INFO && !target.starts_with("promptnite") {
            return;
        }
        let message = cut(fields.message + &fields.rest);

        let mut outbox = self.0.lock().unwrap();
        if let Some(last) = outbox.waiting.last_mut()
            && last.message == message
            && last.target == target
        {
            last.repeats += 1;
        } else if outbox.waiting.len() < MAX_WAITING {
            outbox.waiting.push(Entry {
                time: now_ms(),
                level: match level {
                    Level::ERROR => "error",
                    Level::WARN => "warn",
                    _ => "info",
                },
                target: target.to_string(),
                message,
                repeats: 1,
            });
        }
    }
}

/// Collects an event's text: the message, then any other fields as `key=value`.
#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
    log_target: Option<String>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            name if name.starts_with("log.") => {}
            name => _ = write!(self.rest, " {name}={value:?}"),
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "log.target" => self.log_target = Some(value.to_string()),
            name if name.starts_with("log.") => {}
            name => _ = write!(self.rest, " {name}={value}"),
        }
    }
}

fn cut(message: String) -> String {
    match message.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((end, _)) => message[..end].to_string(),
        None => message,
    }
}

fn log_url() -> Option<String> {
    net::setting("log", "PROMPTNITE_LOG_URL")
        .or_else(page_log_url)
        .filter(|url| !url.is_empty())
}

/// What index.html says, see the top of this file.
fn page_log_url() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()?
            .document()?
            .query_selector("meta[name=promptnite-log-url]")
            .ok()??
            .get_attribute("content")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

fn platform() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        "web"
    } else {
        std::env::consts::OS
    }
}

/// Wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |since| since.as_secs_f64() * 1000.0)
    }
}
