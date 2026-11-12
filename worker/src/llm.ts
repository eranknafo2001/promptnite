// The model call that turns a player's prompt into a Lua bot.
//
// Two wire formats are supported, which covers most providers:
//
//   anthropic  the Claude Messages API, through the official SDK
//   openai     the OpenAI Chat Completions API. Many other providers speak it
//              too; point OPENAI_BASE_URL at them. Plain fetch, so that no
//              particular vendor's SDK is assumed.

import Anthropic from "@anthropic-ai/sdk";

/** Room for thinking plus a script; bot scripts themselves are short. */
const ANTHROPIC_MAX_TOKENS = 8_000;
const OPENAI_BASE_URL = "https://api.openai.com/v1";
/**
 * The module fails a job that takes longer than 45 s (`ATTEMPT_TIMEOUT` in
 * `server/src/lib.rs`). Give up on the model before that, so the player is
 * told what actually went wrong.
 */
const MODEL_TIMEOUT_MS = 35_000;

/**
 * Writing a short bot script is a small task and players are waiting on it,
 * so the defaults are each provider's fast, cheap tier.
 */
const DEFAULT_MODEL = {
  anthropic: "claude-haiku-5-5",
  openai: "gpt-6-luna",
} as const;

type Provider = keyof typeof DEFAULT_MODEL;

export interface ModelEnv {
  PROMPTNITE_PROVIDER?: string;
  PROMPTNITE_MODEL?: string;
  ANTHROPIC_API_KEY?: string;
  OPENAI_API_KEY?: string;
  OPENAI_BASE_URL?: string;
}

/** A script the game rejected, and why. */
export interface Rejected {
  script: string;
  error: string;
}

/** An error whose message is fit to show to the player. */
export class AgentError extends Error {}

const DECLINED = "The agent declined to write a bot for that prompt.";
const CUT_OFF = "The agent's script was cut off for being too long.";

/**
 * The contract between the game and the model. Keep it in sync with
 * `sim/src/lua.rs` (the `state` table) and `sim/src/rules.rs`.
 */
const SYSTEM_PROMPT = `You write the brain of a fighting bot for the game Promptnite.

Bots fight in a square arena. A player describes how their bot should behave and you turn that description into a Lua script. The game runs the script; the player cannot intervene during the match, so the script is the bot's entire behaviour.

## Script contract

Define one global function, \`tick(state)\`. The game calls it once per simulation step and uses the returned table as the bot's action for that step:

- \`{ action = "walk", dx = <number>, dy = <number> }\` walks one step in the direction (dx, dy). Only the direction matters, not the length.
- \`{ action = "attack" }\` punches the nearest enemy within \`state.rules.attack_range\`. It misses if nobody is in range and does nothing while \`state.me.cooldown > 0\`.
- \`{ action = "idle" }\` (or returning nil) does nothing.

A bot does exactly one of these per tick: it cannot walk and attack in the same tick.

\`state\` is rebuilt every tick:

- \`state.tick\`: current step, starting at 1.
- \`state.me\`: \`{ id, x, y, hp, cooldown }\`. \`cooldown\` is the number of ticks until this bot can attack again.
- \`state.enemies\`: array of living enemies, each \`{ id, x, y, hp, dist }\`, where \`dist\` is the distance from this bot. Empty when no enemy is left.
- \`state.arena\`: \`{ half_size }\`. The arena spans -half_size..half_size on both axes and bots cannot leave it. Up to four bots fight, each starting at the middle of a different edge.
- \`state.rules\`: \`{ ticks_per_sec, max_ticks, walk_speed, attack_range, attack_damage, attack_cooldown, max_hp }\`. \`walk_speed\` is in units per second.

The last bot standing wins. If the match reaches \`max_ticks\`, the bot with the most hp wins.

## Environment

- Lua 5.4 syntax with only \`math\`, \`string\`, \`table\` and the base library. There is no \`io\`, \`os\`, \`require\` or \`coroutine\`.
- Globals and upvalues persist between ticks, so the script can remember things.
- \`math.random\` is available and is seeded per match.
- Each call to \`tick\` has a small instruction budget. A call that runs too long, or raises an error, makes the bot idle for that tick, so keep the logic simple and guard against nil (for example an empty \`state.enemies\`).

## Output

Follow the player's description as closely as these rules allow. Where it asks for something a bot cannot do, do the closest thing it can. Reply with the complete script in a single \`\`\`lua code block and nothing else.`;

interface Turn {
  role: "user" | "assistant";
  content: string;
}

/**
 * The worker keeps nothing between jobs, so a retry arrives with the
 * rejected script and is replayed here as the conversation so far.
 */
function conversation(prompt: string, rejected?: Rejected): Turn[] {
  const turns: Turn[] = [
    {
      role: "user",
      content: `Here is how the player wants their bot to behave:\n\n<player_prompt>\n${prompt}\n</player_prompt>`,
    },
  ];
  if (rejected) {
    turns.push(
      { role: "assistant", content: "```lua\n" + rejected.script + "\n```" },
      {
        role: "user",
        content: `The game rejected that script with this error:\n\n${rejected.error}\n\nPlease send a corrected, complete script.`,
      },
    );
  }
  return turns;
}

/**
 * Ask the model for a script. Returns whatever Lua it wrote, unchecked: the
 * game module decides whether it is valid.
 */
export async function writeScript(env: ModelEnv, prompt: string, rejected?: Rejected): Promise<string> {
  const provider = pickProvider(env);
  const model = env.PROMPTNITE_MODEL || DEFAULT_MODEL[provider];
  const turns = conversation(prompt, rejected);

  const text = provider === "anthropic" ? await askAnthropic(env, model, turns) : await askOpenAi(env, model, turns);
  const script = extractLua(text);
  if (!script) throw new AgentError("The agent replied without a script.");
  return script;
}

function pickProvider(env: ModelEnv): Provider {
  const chosen = env.PROMPTNITE_PROVIDER?.toLowerCase();
  if (chosen === "anthropic" || chosen === "claude") return "anthropic";
  if (chosen === "openai") return "openai";
  if (chosen) throw new AgentError(`The agent is misconfigured: unknown provider "${chosen}".`);
  // Not chosen: use whichever provider has a key.
  if (env.ANTHROPIC_API_KEY) return "anthropic";
  if (env.OPENAI_API_KEY) return "openai";
  throw new AgentError("The agent has no model API key.");
}

async function askAnthropic(env: ModelEnv, model: string, turns: Turn[]): Promise<string> {
  if (!env.ANTHROPIC_API_KEY) throw new AgentError("The agent has no Anthropic API key.");
  const client = new Anthropic({
    apiKey: env.ANTHROPIC_API_KEY,
    timeout: MODEL_TIMEOUT_MS,
    // A retry would not fit in the time the module allows for a job.
    maxRetries: 0,
  });

  let reply: Anthropic.Message;
  try {
    reply = await client.messages.create({
      model,
      max_tokens: ANTHROPIC_MAX_TOKENS,
      thinking: { type: "adaptive" },
      output_config: { effort: "medium" },
      system: SYSTEM_PROMPT,
      messages: turns,
    });
  } catch (err) {
    if (err instanceof Anthropic.APIError) {
      throw new AgentError(`Model API error (${err.status ?? "no response"}): ${err.message}`);
    }
    throw err;
  }

  if (reply.stop_reason === "refusal") throw new AgentError(DECLINED);
  if (reply.stop_reason === "max_tokens") throw new AgentError(CUT_OFF);
  // Thinking blocks come first; the script is in the text blocks.
  return reply.content.map((block) => (block.type === "text" ? block.text : "")).join("");
}

async function askOpenAi(env: ModelEnv, model: string, turns: Turn[]): Promise<string> {
  if (!env.OPENAI_API_KEY) throw new AgentError("The agent has no OpenAI API key.");
  const base = (env.OPENAI_BASE_URL || OPENAI_BASE_URL).replace(/\/+$/, "");

  let response: Response;
  try {
    response = await fetch(`${base}/chat/completions`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${env.OPENAI_API_KEY}`,
      },
      body: JSON.stringify({
        model,
        messages: [{ role: "system", content: SYSTEM_PROMPT }, ...turns],
      }),
      signal: AbortSignal.timeout(MODEL_TIMEOUT_MS),
    });
  } catch (err) {
    throw new AgentError(`Could not reach the model API: ${err instanceof Error ? err.message : err}`);
  }

  const reply = (await response.json().catch(() => null)) as any;
  if (!response.ok) {
    throw new AgentError(`Model API error (${response.status}): ${reply?.error?.message ?? "unknown error"}`);
  }
  const choice = reply?.choices?.[0];
  if (choice?.finish_reason === "length") throw new AgentError(CUT_OFF);
  if (choice?.finish_reason === "content_filter" || typeof choice?.message?.refusal === "string") {
    throw new AgentError(DECLINED);
  }
  return typeof choice?.message?.content === "string" ? choice.message.content : "";
}

/** Pull the script out of a ```lua fenced block, tolerating a bare reply. */
export function extractLua(text: string): string {
  const open = text.indexOf("```");
  if (open === -1) return text.trim();
  let rest = text.slice(open + 3);
  if (rest.startsWith("lua")) rest = rest.slice(3);
  const close = rest.indexOf("```");
  return (close === -1 ? rest : rest.slice(0, close)).trim();
}
