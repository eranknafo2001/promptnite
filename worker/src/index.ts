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

import { AgentError, writeScript, type ModelEnv, type Rejected } from "./llm";

interface Env extends ModelEnv {
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

    ctx.waitUntil(run(env, job));
    return new Response("accepted\n", { status: 202 });
  },
} satisfies ExportedHandler<Env>;

async function run(env: Env, job: Job): Promise<void> {
  try {
    const script = await writeScript(env, job.prompt, job.rejected);
    await callReducer(env, "deliver_script", [env.AGENT_SECRET, job.job_id, script]);
  } catch (err) {
    // Only AgentError messages are written for players; anything else is a
    // bug or an outage and goes to the Worker's log.
    if (!(err instanceof AgentError)) console.error(`job ${job.job_id}:`, err);
    const message = err instanceof AgentError ? err.message : "The agent ran into a problem. Try again.";
    await callReducer(env, "fail_job", [env.AGENT_SECRET, job.job_id, message]).catch((err) =>
      console.error(`job ${job.job_id}: could not report the failure:`, err),
    );
  }
}

/** Call a reducer through SpacetimeDB's HTTP API. Arguments go as a JSON array. */
async function callReducer(env: Env, reducer: string, args: unknown[]): Promise<void> {
  const base = env.SPACETIMEDB_URI.replace(/\/+$/, "");
  const response = await fetch(`${base}/v1/database/${env.SPACETIMEDB_DB}/call/${reducer}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args),
  });
  if (!response.ok) {
    // Typically "No such job": the player resubmitted or the round moved on.
    console.warn(`${reducer} was refused (${response.status}): ${await response.text()}`);
  }
}
