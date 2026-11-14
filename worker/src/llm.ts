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
 * A Worker may keep running for about 30 s after it answered the module.
 * Give up on the model before that, so there is time left to report it.
 */
const MODEL_TIMEOUT_MS = 24_000;
/**
 * How hard the model thinks before it answers. A bot script is short and
 * simple, and at the default effort a reasoning model can think past
 * `MODEL_TIMEOUT_MS`, which fails the player's prompt.
 */
const ANTHROPIC_EFFORT = "low";
const OPENAI_REASONING_EFFORT = "low";

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
const TOO_SLOW = "The agent took too long. Try again.";

/**
 * The contract between the game and the model. Keep it in sync with
 * `sim/src/lua.rs` (the `state` table) and `sim/src/rules.rs`.
 */
const SYSTEM_PROMPT = `You write the brain of a fighting bot for the game Promptnite.

Bots fight in a square arena. A player describes how their bot should behave and you turn that description into a Lua script. The game runs the script; the player cannot intervene during the match, so the script is the bot's entire behaviour.

## Loadout

Declare a global build = { hp = 2, speed = 2, damage = 2, range = 2, cooldown = 2 } before tick. Each field is an integer 0–5; total at most 10. Choose allocations to match the player’s strategy. Invalid builds are rejected. Missing build preserves legacy balanced stats.
Stats: hp = 60 + 20*hp; walk_speed = 2.2 + 0.4*speed; attack_damage = 6 + 2*damage; attack_range = 1.2 + 0.15*range; attack_cooldown = 7 - cooldown ticks. A swing occurs every attack_cooldown + 1 ticks. Stats are frozen at load time.
Both state.me and each enemy expose max_hp, walk_speed, attack_damage, attack_range, attack_cooldown. state.rules contains your own combat stats. Adapt to enemy builds.

## Script contract

Define one global function, \`tick(state)\`. The game calls it once per simulation step and uses the returned table as the bot's action for that step:

- \`{ action = "walk", dx = <number>, dy = <number> }\` walks one step in the direction (dx, dy). Only the direction matters, not the length.
- \`{ action = "dash", dx = <number>, dy = <number> }\` is a walk that covers three times the distance (\`state.rules.dash_speed\`) and costs \`state.rules.dash_stamina_cost\` stamina. With less stamina than that it is an ordinary walk.
- \`{ action = "attack" }\` punches the nearest enemy within \`state.rules.attack_range\` and costs \`state.rules.attack_stamina_cost\` stamina, hit or miss. It misses if nobody is in range and does nothing while \`state.me.cooldown > 0\` or \`state.me.stamina\` is below the cost.
- \`{ action = "idle" }\` (or returning nil) does nothing.

A bot does exactly one of these per tick: it cannot walk and attack in the same tick.

There is no teleport action. Teleporting happens to a bot: it is part of moving, so each tick it comes after every bot has walked or dashed and before any punch lands. A bot that is then closer than \`state.rules.teleporter_radius\` to a pad's centre is moved to the exact centre of pad \`to\`, whether it walked there or an enemy pushed it there (bots are solid and push each other). To use a pad, walk towards its \`x, y\`; to avoid one, keep \`dist\` above the radius. The pad a bot comes out of does not take it until it has walked off that pad; walking back on then sends it one pad further. To go from pad A to a pad that A does not lead to, ride the ring: step off each pad you come out of and back on. Enemies teleport the same way, so one whose position suddenly changes has gone through a pad.

## Stamina

Every bot, whatever its build, starts with \`state.rules.max_stamina\` (100) stamina and never holds more. A dash costs 20 and a swing costs 15. On every tick a bot spends none (walking, idling, or asking for something it cannot do) it gets \`state.rules.stamina_regen\` (2) back. A bot that swings every time its cooldown allows spends more than it gets back and runs dry, after which it can only swing every few ticks, so pace attacks and dashes: check \`state.me.stamina\` before relying on either, and back off or walk to recover. Enemies' stamina is visible, so a bot can tell when an enemy is too tired to swing or dash.

\`state\` is rebuilt every tick:

- \`state.tick\`: current step, starting at 1.
- \`state.me\`: \`{ id, x, y, hp, stamina, cooldown }\`. \`cooldown\` is the number of ticks until this bot can attack again.
- \`state.enemies\`: array of living enemies, each \`{ id, x, y, hp, stamina, dist }\`, where \`dist\` is the distance from this bot. Empty when no enemy is left.
- \`state.teleporters\`: array of teleporter pads, each \`{ id, x, y, dist, to }\`, sorted by \`id\`. The room's owner places them, so there may be none. Players see each pad's \`id\` as the number on it and may refer to pads by it. \`to\` is the \`id\` of the pad this one leads to: always the next \`id\` in the array, and the first one after the last. It is nil when there is only one pad, which does nothing.
- \`state.arena\`: \`{ half_size }\`. The arena spans -half_size..half_size on both axes and bots cannot leave it. Up to four bots fight, each starting at the middle of a different edge.
- \`state.rules\`: \`{ ticks_per_sec, max_ticks, walk_speed, attack_range, attack_damage, attack_cooldown, max_hp, max_stamina, stamina_regen, attack_stamina_cost, dash_stamina_cost, dash_speed, teleporter_radius }\`. \`walk_speed\` and \`dash_speed\` are in units per second.

The last bot standing wins. If the match reaches \`max_ticks\`, the bot with the most hp wins.

## Environment

- Lua 5.4 syntax with only \`math\`, \`string\`, \`table\` and the base library. There is no \`io\`, \`os\`, \`require\` or \`coroutine\`.
- String concatenation (\`..\`) is not available, and a string literal may be at most 256 bytes. A script that uses either is rejected. Bots work with numbers and tables; they have no use for building strings.
- Globals and upvalues persist between ticks, so the script can remember things.
- \`math.random\` is available and is seeded per match.
- Each call to \`tick\` has a small instruction budget. A call that runs too long, or raises an error, makes the bot idle for that tick, so keep the logic simple and guard against nil (for example an empty \`state.enemies\`).
- Memory is limited to 1 MB, and instructions cost more the more memory the script holds. Remember a few numbers between ticks, not a growing history: a bot that goes over the limit is switched off for the rest of the match.

## Output

Follow the player's description as closely as these rules allow. Where it asks for something a bot cannot do, do the closest thing it can. Reply with the complete script in a single \`\`\`lua code block and nothing else.`;

interface Turn {
  role: "user" | "assistant";
  content: string;
}

/**
 * The Worker keeps nothing between requests, so a retry arrives with the
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
    // A retry would not fit in the time a Worker has left.
    maxRetries: 0,
  });

  let reply: Anthropic.Message;
  try {
    reply = await client.messages.create({
      model,
      max_tokens: ANTHROPIC_MAX_TOKENS,
      thinking: { type: "adaptive" },
      output_config: { effort: ANTHROPIC_EFFORT },
      system: SYSTEM_PROMPT,
      messages: turns,
    });
  } catch (err) {
    if (err instanceof Anthropic.APIConnectionTimeoutError) throw new AgentError(TOO_SLOW);
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

  // One deadline for the whole call, including the second request below.
  const signal = AbortSignal.timeout(MODEL_TIMEOUT_MS);
  const ask = async (extra: Record<string, unknown>) => {
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
          ...extra,
        }),
        signal,
      });
    } catch (err) {
      if (signal.aborted) throw new AgentError(TOO_SLOW);
      throw new AgentError(`Could not reach the model API: ${err instanceof Error ? err.message : err}`);
    }
    const reply = (await response.json().catch(() => null)) as any;
    // The deadline can also pass while the body is still arriving.
    if (signal.aborted) throw new AgentError(TOO_SLOW);
    return { response, reply };
  };

  let { response, reply } = await ask({ reasoning_effort: OPENAI_REASONING_EFFORT });
  if (response.status === 400 && JSON.stringify(reply ?? "").includes("reasoning_effort")) {
    // A model or service without that setting turns the request down at
    // once, so asking again without it costs next to nothing.
    ({ response, reply } = await ask({}));
  }
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
