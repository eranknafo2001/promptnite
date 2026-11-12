// Logging.
//
// The game's three parts log to one Axiom dataset, so a single query can
// follow a job or a room across them. Every event has the same core fields:
//
//   _time    when it happened
//   source   "worker" | "module" | "client"
//   level    "error" | "warn" | "info" | "debug" | "trace"
//   message
//
// plus whatever ids apply: job_id, room_id, player_id, identity.
//
// The module sends its own events (server/src/lib.rs). This Worker sends its
// own, and relays the game client's (`POST /logs`), because an Axiom token in
// the client could be read by anyone who downloads it.
//
// Without AXIOM_TOKEN nothing is sent; the Worker's own events are still
// printed, for `wrangler dev` and `wrangler tail`.

export interface LogEnv {
  /** An Axiom API token that may ingest into the dataset. Unset: no shipping. */
  AXIOM_TOKEN?: string;
  AXIOM_DATASET?: string;
  /** The dataset's edge deployment, see Axiom's Settings > General. */
  AXIOM_DOMAIN?: string;
  /** Limits `POST /logs` per address. */
  LOG_LIMIT?: RateLimit;
}

const DEFAULT_DATASET = "promptnite";
const DEFAULT_DOMAIN = "us-east-1.aws.edge.axiom.co";

const LEVELS = ["error", "warn", "info", "debug", "trace"] as const;
export type Level = (typeof LEVELS)[number];
export type Fields = Record<string, unknown>;

export interface LogEvent extends Fields {
  _time: string;
  source: "worker" | "client";
  level: Level;
  message: string;
}

/** Collects one request's events; `flush` sends them in a single batch. */
export class Logger {
  private pending: LogEvent[] = [];

  /** `fields` go on every event, e.g. the job being worked on. */
  constructor(
    private env: LogEnv,
    private fields: Fields = {},
  ) {}

  info(message: string, fields?: Fields): void {
    this.write("info", message, fields);
  }

  warn(message: string, fields?: Fields): void {
    this.write("warn", message, fields);
  }

  error(message: string, fields?: Fields): void {
    this.write("error", message, fields);
  }

  private write(level: Level, message: string, fields: Fields = {}): void {
    const all = { ...this.fields, ...fields };
    console[level](message, JSON.stringify(all));
    this.pending.push({ _time: new Date().toISOString(), source: "worker", level, message, ...all });
  }

  /** Queue events written elsewhere (the game client) as they are. */
  relay(events: LogEvent[]): void {
    this.pending.push(...events);
  }

  /** Send what was collected. Never throws: logging must not fail a request. */
  async flush(): Promise<void> {
    const events = this.pending;
    this.pending = [];
    if (events.length === 0 || !this.env.AXIOM_TOKEN) return;

    const domain = this.env.AXIOM_DOMAIN || DEFAULT_DOMAIN;
    const dataset = this.env.AXIOM_DATASET || DEFAULT_DATASET;
    try {
      const response = await fetch(`https://${domain}/v1/ingest/${encodeURIComponent(dataset)}`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${this.env.AXIOM_TOKEN}`,
        },
        body: JSON.stringify(events),
        signal: AbortSignal.timeout(5_000),
      });
      if (!response.ok) {
        // Printed only: sending this to Axiom would fail the same way.
        console.error(`Axiom refused ${events.length} events (${response.status}): ${await response.text()}`);
      }
    } catch (err) {
      console.error(`could not reach Axiom, ${events.length} events lost:`, err);
    }
  }
}

// Client logs ---------------------------------------------------------------

/** One request from a client is a few hundred bytes; this is generous. */
const MAX_BODY_BYTES = 64_000;
const MAX_EVENTS = 50;
const MAX_MESSAGE_CHARS = 2_000;
/** A client's clock is believed if it is this close to ours. */
const CLOCK_PAST_MS = 60 * 60 * 1000;
const CLOCK_FUTURE_MS = 5 * 60 * 1000;

/** Any page may post logs (the client runs on itch.io's domain and locally). */
const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "POST",
  "access-control-allow-headers": "content-type",
  "access-control-max-age": "86400",
};

/**
 * `POST /logs`: a batch of events from a game client, relayed to Axiom.
 *
 *   { session, identity?, room_id?, player_id?, platform?, build?,
 *     events: [{ time, level, target, message, repeats }] }
 *
 * Anyone can call this, so nothing in the body is trusted: every field is
 * checked and cut to size, and each address may only send so many batches.
 */
export async function clientLogs(request: Request, env: LogEnv, ctx: ExecutionContext): Promise<Response> {
  const reply = (status: number) => new Response(null, { status, headers: CORS });
  if (request.method === "OPTIONS") return reply(204);
  if (request.method !== "POST") return reply(405);

  const address = request.headers.get("cf-connecting-ip") ?? "unknown";
  if (env.LOG_LIMIT && !(await env.LOG_LIMIT.limit({ key: address })).success) return reply(429);

  if (Number(request.headers.get("content-length")) > MAX_BODY_BYTES) return reply(413);
  const text = await request.text();
  if (text.length > MAX_BODY_BYTES) return reply(413);
  // Not `request.json()`: a panicking browser client sends its last words
  // with `sendBeacon`, which labels them text/plain.
  let batch: any;
  try {
    batch = JSON.parse(text);
  } catch {
    return reply(400);
  }
  if (typeof batch !== "object" || batch === null || !Array.isArray(batch.events)) return reply(400);

  const context: Fields = {
    session: short(batch.session, 40),
    identity: short(batch.identity, 80),
    room_id: integer(batch.room_id),
    player_id: integer(batch.player_id),
    platform: short(batch.platform, 20),
    build: short(batch.build, 40),
    country: short(request.cf?.country, 2),
  };
  const now = Date.now();
  const events = batch.events.slice(0, MAX_EVENTS).flatMap((event: any): LogEvent[] => {
    const message = short(event?.message, MAX_MESSAGE_CHARS);
    if (!message) return [];
    const time = Number(event.time);
    const believable = time > now - CLOCK_PAST_MS && time < now + CLOCK_FUTURE_MS;
    return [
      {
        _time: new Date(believable ? time : now).toISOString(),
        source: "client",
        level: LEVELS.includes(event.level) ? event.level : "info",
        message,
        target: short(event.target, 100),
        // How many times in a row the client logged this.
        repeats: integer(event.repeats),
        ...context,
      },
    ];
  });

  const log = new Logger(env);
  log.relay(events);
  ctx.waitUntil(log.flush());
  return reply(204);
}

/** A non-empty string cut to `max` characters, or nothing. */
function short(value: unknown, max: number): string | undefined {
  return typeof value === "string" && value.length > 0 ? value.slice(0, max) : undefined;
}

function integer(value: unknown): number | undefined {
  return Number.isSafeInteger(value) ? (value as number) : undefined;
}
