// Promptnite agent worker (Cloudflare Worker + Durable Object).
//
// A pipe between the game and a model, and nothing more. It connects to the
// module like any other client and takes its work from a queue:
//
//   module: agent_job row ──► `agent_work` subscription ──► here ──► model API
//   module ◄──────── deliver_script / fail_job reducers ◄── here
//
// A plain Worker only lives for one request, and a subscription needs
// somebody to stay connected. That is the `Agent` Durable Object below: one
// instance that holds a WebSocket to the database.
//
// Deployed, the object is only awake while there is work. A few seconds
// after the last of it, it disconnects and Cloudflare puts it to sleep. When the next
// prompt comes in, the module sends a request here (its AGENT_URL), which
// wakes the object; it connects and is given the jobs that were waiting.
//
// On your machine the module can't reach us (SpacetimeDB refuses to call
// local addresses), so `npm run dev` sets STAY_CONNECTED and the object
// never disconnects.
//
// The SpacetimeDB TypeScript SDK can't be used for that: it builds its
// serializers with `Function(...)`, which Workers forbid. So this speaks the
// database's JSON WebSocket protocol directly, the few messages it needs.
//
// Scripts are NOT checked here. The module validates what we deliver, and if
// it rejects a script the same job shows up again with the next `attempt`,
// carrying the script and the error. Nothing is kept between jobs.

import { DurableObject } from "cloudflare:workers";
import { AgentError, writeScript, type ModelEnv } from "./llm";

interface Env extends ModelEnv {
  AGENT: DurableObjectNamespace<Agent>;
  /** Same value the module was published with (its AGENT_SECRET). */
  AGENT_SECRET?: string;
  /** Where the module lives, e.g. https://maincloud.spacetimedb.com */
  SPACETIMEDB_URI: string;
  SPACETIMEDB_DB: string;
  /** Set to anything to never go to sleep. For when the module can't wake us. */
  STAY_CONNECTED?: string;
}

/**
 * How often the object checks that the database still answers. The alarm is
 * also what keeps the object awake, and brings it back after Cloudflare
 * restarts it.
 */
const KEEPALIVE_MS = 20_000;
/**
 * How long to stay connected after the last piece of work. Long enough to
 * still be there for the rest of a room submitting, short enough not to be
 * billed for waiting.
 */
const IDLE_MS = 5_000;

// Any request wakes the object. That is all the module's call is for: the
// work itself arrives over the object's own connection.
export default {
  async fetch(_request: Request, env: Env): Promise<Response> {
    return new Response(`promptnite agent: ${await agent(env).knock()}\n`);
  },
} satisfies ExportedHandler<Env>;

function agent(env: Env): DurableObjectStub<Agent> {
  return env.AGENT.get(env.AGENT.idFromName("agent"));
}

// ---- What the database sends (only the parts read here) --------------------

interface TableUpdates {
  tables: { table_name: string; updates: { inserts: string[] }[] }[];
}

interface ServerMessage {
  /** The outcome of a reducer we called, with the rows it changed. */
  TransactionUpdate?: {
    status: { Committed?: TableUpdates; Failed?: string };
    reducer_call: { request_id: number };
  };
  /** Rows changed by somebody else. */
  TransactionUpdateLight?: { update: TableUpdates };
  SubscriptionError?: { error: string };
}

/** A row of `agent_work`: the columns of `AgentJob` in server/src/lib.rs, in order. */
type JobRow = [
  id: number,
  playerId: number,
  worker: unknown,
  round: number,
  prompt: string,
  attempt: number,
  rejectedScript: string,
  rejectedError: string,
];

export class Agent extends DurableObject<Env> {
  /** The connection to the database, from the moment we start dialling. */
  #socket?: WebSocket;
  /** Whether the module has accepted us as a worker on this connection. */
  #ready = false;
  /** Whether the database sent anything since `#checkedAt`. */
  #heard = false;
  #checkedAt = 0;
  /** Reducer calls waiting for their outcome, by request id. */
  #calls = new Map<number, { done: () => void; failed: (err: Error) => void }>();
  #requests = 0;
  /** Jobs the model is writing right now. */
  #running = 0;
  /** When we last had a reason to be awake. */
  #lastWork = Date.now();
  #status = "starting";

  /** Somebody wants us awake. */
  async knock(): Promise<string> {
    this.#lastWork = Date.now();
    return this.#wake();
  }

  /** Make sure we are connected, and that we will check again. */
  async #wake(): Promise<string> {
    if (!this.#socket) this.#connect();
    await this.#setAlarm();
    return this.#status;
  }

  /** Whether we may go to sleep, and if so, how long until then. */
  #idleIn(): number | undefined {
    // A connection that is still signing on is about to be given work.
    const waiting = this.#socket && !this.#ready;
    if (this.env.STAY_CONNECTED || this.#running > 0 || waiting) return undefined;
    return Math.max(0, this.#lastWork + IDLE_MS - Date.now());
  }

  async #setAlarm(): Promise<void> {
    await this.ctx.storage.setAlarm(Date.now() + Math.min(KEEPALIVE_MS, this.#idleIn() ?? KEEPALIVE_MS));
  }

  async alarm(): Promise<void> {
    const socket = this.#socket;
    if (this.#idleIn() === 0) {
      // Nothing to do. With no connection and no alarm, Cloudflare puts the
      // object to sleep until the module knocks.
      if (socket) this.#drop(socket, "asleep until the next prompt");
      return;
    }
    if (socket && Date.now() - this.#checkedAt >= KEEPALIVE_MS) {
      if (!this.#heard) {
        // A connection can die without a close event. We would sit here
        // looking connected while the module has long written us off.
        this.#drop(socket, "the database stopped answering");
      } else if (this.#ready) {
        // Registering twice changes nothing, so it doubles as the question
        // whose answer the next check looks for.
        this.#heard = false;
        this.#checkedAt = Date.now();
        this.#call(socket, "register_worker", this.env.AGENT_SECRET).catch(() => {});
      }
    }
    await this.#wake();
  }

  #connect(): void {
    const { AGENT_SECRET: secret, SPACETIMEDB_URI: uri, SPACETIMEDB_DB: db } = this.env;
    if (!secret) {
      this.#say("AGENT_SECRET is not set. Use the value the module was published with.");
      return;
    }
    this.#say(`connecting to ${db} at ${uri}`);
    let socket: WebSocket;
    try {
      const url = `${uri.replace(/^http/, "ws").replace(/\/+$/, "")}/v1/database/${db}/subscribe`;
      socket = new WebSocket(url, "v1.json.spacetimedb");
    } catch (err) {
      this.#say(`could not connect to ${db} at ${uri}: ${message(err)}`);
      return;
    }
    this.#socket = socket;
    this.#ready = false;
    this.#heard = false;
    this.#checkedAt = Date.now();
    socket.addEventListener("open", () => {
      socket.send(JSON.stringify({ Subscribe: { query_strings: ["SELECT * FROM agent_work"], request_id: 0 } }));
      // The module only hands jobs to workers that know the secret.
      this.#call(socket, "register_worker", secret).then(
        () => {
          if (this.#socket !== socket) return;
          this.#ready = true;
          this.#lastWork = Date.now();
          this.#say(`connected to ${db} at ${uri}, waiting for prompts`);
          return this.#setAlarm();
        },
        (err) => this.#drop(socket, `the module turned this worker away (${message(err)}). Same AGENT_SECRET on both?`),
      );
    });
    socket.addEventListener("message", (event) => this.#receive(socket, event.data));
    // The module takes our open jobs back when we drop off, so there is
    // nothing to resume. The next alarm connects again.
    socket.addEventListener("close", (event) => this.#drop(socket, `disconnected (${event.reason || event.code})`));
    socket.addEventListener("error", () => this.#drop(socket, `could not connect to ${db} at ${uri}`));
  }

  /** Give up on a connection. Does nothing if it was already replaced. */
  #drop(socket: WebSocket, why: string): void {
    if (this.#socket !== socket) return;
    this.#socket = undefined;
    this.#say(why);
    for (const call of this.#calls.values()) call.failed(new Error("the connection was lost"));
    this.#calls.clear();
    try {
      socket.close();
    } catch {
      // It never opened, or is closed already.
    }
  }

  #receive(socket: WebSocket, data: unknown): void {
    if (this.#socket !== socket || typeof data !== "string") return;
    this.#heard = true;
    const msg = JSON.parse(data) as ServerMessage;
    if (msg.SubscriptionError) {
      this.#drop(socket, `the work queue subscription failed: ${msg.SubscriptionError.error}`);
      return;
    }
    const outcome = msg.TransactionUpdate;
    if (outcome) {
      const call = this.#calls.get(outcome.reducer_call.request_id);
      this.#calls.delete(outcome.reducer_call.request_id);
      if (outcome.status.Committed) call?.done();
      else call?.failed(new Error(outcome.status.Failed ?? "the database refused the call"));
    }
    // A job is a new row in `agent_work`. A retry is one too: the rejected
    // row goes away and the next attempt is inserted. Nothing can be ours
    // from before we subscribed: every connection is a new identity, and
    // jobs are only given to a worker when it registers or after.
    const update = outcome?.status.Committed ?? msg.TransactionUpdateLight?.update;
    for (const table of update?.tables ?? []) {
      if (table.table_name !== "agent_work") continue;
      for (const { inserts } of table.updates) {
        for (const row of inserts) void this.#run(socket, JSON.parse(row) as JobRow);
      }
    }
  }

  /** Call a reducer; settles once the database says how it went. */
  #call(socket: WebSocket, reducer: string, ...args: unknown[]): Promise<void> {
    return new Promise((done, failed) => {
      const request_id = ++this.#requests;
      socket.send(JSON.stringify({ CallReducer: { reducer, args: JSON.stringify(args), request_id, flags: 0 } }));
      this.#calls.set(request_id, { done, failed });
    });
  }

  /** One attempt at one job: ask the model, hand the module whatever it wrote. */
  async #run(socket: WebSocket, [id, , , , prompt, attempt, rejectedScript, rejectedError]: JobRow): Promise<void> {
    const name = `job ${id} (attempt ${attempt})`;
    console.log(`${name}: writing`);
    this.#running++;
    const rejected = rejectedScript ? { script: rejectedScript, error: rejectedError } : undefined;
    try {
      const script = await writeScript(this.env, prompt, rejected);
      await this.#call(socket, "deliver_script", id, script);
      console.log(`${name}: delivered`);
    } catch (err) {
      // Only AgentError messages are written for players; anything else is a
      // bug or an outage and stays in this log. That includes the module
      // refusing our answer, typically "No such job": the player resubmitted,
      // the job timed out or the round moved on. Reporting that fails the
      // same way, which is fine.
      console.error(`${name}:`, err instanceof AgentError ? err.message : err);
      const error = err instanceof AgentError ? err.message : "The agent ran into a problem. Try again.";
      await this.#call(socket, "fail_job", id, error).catch((err) =>
        console.warn(`${name}: could not report the failure: ${message(err)}`),
      );
    } finally {
      this.#running--;
      this.#lastWork = Date.now();
      await this.#setAlarm();
    }
  }

  #say(status: string): void {
    this.#status = status;
    console.log(status);
  }
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
