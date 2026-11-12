// Promptnite agent worker (Cloudflare Worker).
//
// A pipe between the game and a model, and nothing more:
//
//   module ── POST /job ──► here ──► model API
//   module ◄── deliver_script / fail_job (SpacetimeDB HTTP API) ── here
//
// The module can only wait 500 ms for an HTTP reply, so /job answers 202 at
// once and the model call carries on in the background (`waitUntil`).
//
// Scripts are NOT checked here. The module validates what we deliver, and if
// it rejects a script it sends a new job that includes the script and the
// error. This Worker keeps no state between requests.
//
// It also takes the game client's logs (`POST /logs`) and passes them on to
// Axiom together with its own, see log.ts.

import { AgentError, writeScript, type ModelEnv, type Rejected } from "./llm";
import { clientLogs, Logger, type LogEnv } from "./log";

interface Env extends ModelEnv, LogEnv {
  /** Shared with the module (its AGENT_SECRET). Proves each side to the other. */
  AGENT_SECRET?: string;
  /** Where the module lives, e.g. https://maincloud.spacetimedb.com */
  SPACETIMEDB_URI: string;
  SPACETIMEDB_DB: string;
}

interface Job {
  job_id: number;
  prompt: string;
  rejected?: Rejected;
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/logs") return clientLogs(request, env, ctx);
    if (request.method !== "POST" || url.pathname !== "/job") {
      return new Response("promptnite agent\n", { status: 404 });
    }
    if (!env.AGENT_SECRET || request.headers.get("authorization") !== `Bearer ${env.AGENT_SECRET}`) {
      return new Response("unauthorized\n", { status: 401 });
    }

    const job = (await request.json().catch(() => null)) as Job | null;
    if (typeof job?.job_id !== "number" || typeof job.prompt !== "string") {
      return new Response("bad job\n", { status: 400 });
    }

    const log = new Logger(env, { job_id: job.job_id });
    ctx.waitUntil(run(env, job, log).finally(() => log.flush()));
    return new Response("accepted\n", { status: 202 });
  },
} satisfies ExportedHandler<Env>;

async function run(env: Env, job: Job, log: Logger): Promise<void> {
  const started = Date.now();
  const retry = job.rejected !== undefined;
  try {
    const script = await writeScript(env, job.prompt, job.rejected);
    log.info("script written", { retry, ms: Date.now() - started, script_chars: script.length });
    await callReducer(env, log, "deliver_script", [env.AGENT_SECRET, job.job_id, script]);
  } catch (err) {
    // Only AgentError messages are written for players; anything else is a
    // bug or an outage.
    const ms = Date.now() - started;
    if (err instanceof AgentError) {
      log.warn("agent failed", { retry, ms, error: err.message });
    } else {
      log.error("agent crashed", { retry, ms, error: String(err), stack: err instanceof Error ? err.stack : undefined });
    }
    const message = err instanceof AgentError ? err.message : "The agent ran into a problem. Try again.";
    await callReducer(env, log, "fail_job", [env.AGENT_SECRET, job.job_id, message]).catch((err) =>
      log.error("could not report the failure", { error: String(err) }),
    );
  }
}

/** Call a reducer through SpacetimeDB's HTTP API. Arguments go as a JSON array. */
async function callReducer(env: Env, log: Logger, reducer: string, args: unknown[]): Promise<void> {
  const base = env.SPACETIMEDB_URI.replace(/\/+$/, "");
  const response = await fetch(`${base}/v1/database/${env.SPACETIMEDB_DB}/call/${reducer}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args),
  });
  if (!response.ok) {
    // Typically "No such job": the player resubmitted or the round moved on.
    log.warn("reducer refused", { reducer, status: response.status, reply: await response.text() });
  }
}
