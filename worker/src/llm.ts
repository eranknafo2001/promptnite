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
Stats: hp = 60 + 20*hp; walk_speed = 2.2 + 0.4*speed; attack_damage = 6 + 2*damage; attack_range = 1.2 + 0.15*range; attack_cooldown = 7 - cooldown ticks, counted from the tick a swing lands. Stats are frozen at load time.
Both state.me and each enemy expose max_hp, walk_speed, attack_damage, attack_range, attack_cooldown. state.rules contains your own combat stats. Adapt to enemy builds.

## Script contract

Define one global function, \`tick(state)\`. The game calls it once per simulation step and uses the returned table as the bot's action for that step:

- \`{ action = "walk", dx = <number>, dy = <number> }\` walks one step in the direction (dx, dy). Only the direction matters, not the length.
- \`{ action = "dash", dx = <number>, dy = <number> }\` is a walk that covers 3.6 times the distance (\`state.rules.dash_speed\`) and costs \`state.rules.dash_stamina_cost\` stamina. With less stamina than that it is an ordinary walk.
- \`{ action = "shield", dx = <number>, dy = <number> }\` holds a shield up for this tick and costs \`state.rules.shield_stamina_cost\` stamina. No attack damages the bot on a tick its shield is up. \`dx\` and \`dy\` are optional: with them the bot also moves, at half its walking speed (\`state.rules.shield_speed\`); without them it stands still. With less stamina than the cost, or while \`state.me.shield_cooldown > 0\`, the shield stays down and the action is an ordinary walk (or nothing, without a direction). The shield only lasts for ticks that return this action, so return it every tick it should stay up.
- \`{ action = "attack" }\` starts winding up a swing and costs \`state.rules.attack_stamina_cost\` stamina. It does nothing while \`state.me.winding > 0\`, \`state.me.cooldown > 0\` or \`state.me.stamina\` is below the cost. See "Swings" below.
- \`{ action = "idle" }\` (or returning nil) does nothing.

A bot does exactly one of these per tick: it cannot walk and start a swing in the same tick, or attack from behind its shield.

## Swings

A swing does not land when it is asked for. The bot winds up for \`state.rules.attack_windup\` (3) ticks, and every enemy can see it: \`winding\` counts 3, 2, 1, and the swing lands on the tick a script sees \`winding == 1\`. It hits the nearest enemy that is within \`attack_range\` at that moment, after everybody has moved, so where the target was when the wind-up started does not matter. A swing with nobody in range is a miss, and the stamina is gone. After it lands, hit or miss, the bot waits \`attack_cooldown\` ticks before it can start another.

While winding up, a bot can only walk, at its normal speed: \`dash\` and \`shield\` are plain walks and \`attack\` does nothing. So return a walk towards the target every tick of your own wind-up, or it will step out of range.

A hit does \`attack_damage\` and throws the target \`state.rules.knockback\` (2.5) units straight away from the attacker. Two things follow:

- **Ring-out.** A bot thrown past the railing is out of the match at once, whatever hp it had. Bots cannot walk or be pushed over it, only thrown. A bot's centre can go no further than \`state.arena.half_size - 0.9\` on either axis, so one standing within \`knockback\` of that is one hit from losing, and one thrown there is a target. Fight away from the railing, and get behind an enemy to throw it towards the nearest one. Every bot starts right next to the railing.
- **A hit breaks a wind-up.** A bot that takes damage while winding up loses its swing (and the stamina it paid). Of two bots swinging at each other, the one that started first lands and the other does not; started on the same tick, both land. So do not answer a wind-up you can see with your own swing: get out of it or block it, then swing while the enemy's \`cooldown\` is running, when it can neither break your wind-up nor swing back in time.

There are three answers to an enemy's wind-up:

- **Walk out of range.** Free, but the enemy walks after you during its wind-up, so it only works with more \`walk_speed\` than the enemy has, or from the edge of its range.
- **Dash out of range** on the last tick or two. Always fast enough against an enemy that only walks, costs a dash.
- **Shield** on the tick it lands (\`enemy.winding == 1\`). Takes all the damage, but a blocked hit costs \`state.rules.shield_block_stamina_cost\` (20) stamina on top of the shield's own cost, and still throws the bot half as far, over the railing too. With less stamina than that the block still holds and stamina drops to 0. **After a block the shield is locked out** for \`state.rules.shield_lockout\` (20) ticks, 2 seconds: \`shield_cooldown\` counts them down and the shield cannot go up until it is 0. A block buys one hit, not two: right after one, get out of range (walk, or dash) instead of waiting for the next swing, or swing back only if the enemy's \`cooldown\` leaves time to finish before its next swing lands.

Defending costs more stamina than swinging, so a bot that only defends runs dry and then gets hit. A good bot does both: it makes the enemy miss or pays for a block, and swings back while the enemy recovers.

There is no teleport action. Teleporting happens to a bot: it is part of moving, so each tick it comes after every bot has walked or dashed and before any punch lands. A bot that is then closer than \`state.rules.teleporter_radius\` to a pad's centre is moved to the exact centre of pad \`to\`, whether it walked there or an enemy pushed it there (bots are solid and push each other). To use a pad, walk towards its \`x, y\`; to avoid one, keep \`dist\` above the radius. The pad a bot comes out of does not take it until it has walked off that pad; walking back on then sends it one pad further. To go from pad A to a pad that A does not lead to, ride the ring: step off each pad you come out of and back on. Enemies teleport the same way, so one whose position suddenly changes has gone through a pad.

## Stamina

Every bot, whatever its build, starts with \`state.rules.max_stamina\` (100) stamina and never holds more. A dash costs 20, a swing costs 15 and each tick behind the shield costs 5. On every tick a bot spends none (walking, idling, or asking for something it cannot do) it gets \`state.rules.stamina_regen\` (2) back. Check \`state.me.stamina\` before relying on a swing, a dash or a block, and back off to recover when it is low. A shield held without a break empties full stamina in 20 ticks and leaves nothing to swing with, so raise it only on the tick an enemy's swing lands, not all the time. Enemies' stamina is visible, so a bot can tell when an enemy is too tired to swing, dash or shield, and \`shielding\` tells whether an enemy's shield was up on the previous tick: a swing at a shielded enemy is wasted. An enemy whose \`shield_cooldown\` is above 0 cannot block: that is the time to press it.

\`state\` is rebuilt every tick:

- \`state.tick\`: current step, starting at 1.
- \`state.me\`: \`{ id, x, y, hp, stamina, shielding, cooldown, winding, shield_cooldown }\`. \`shielding\` is true if the shield was up on the previous tick. \`cooldown\` is the number of ticks until this bot can start another swing. \`winding\` is 0, or the number of ticks until the swing it is winding up lands. \`shield_cooldown\` is the number of ticks until this bot can raise its shield again after a block (0: it can).
- \`state.enemies\`: array of living enemies, each \`{ id, x, y, hp, stamina, shielding, cooldown, winding, shield_cooldown, dist }\`, where \`dist\` is the distance from this bot and the rest mean what they mean in \`state.me\`. Empty when no enemy is left.
- \`state.teleporters\`: array of teleporter pads, each \`{ id, x, y, dist, to }\`, sorted by \`id\`. The room's owner places them, so there may be none. Players see each pad's \`id\` as the number on it and may refer to pads by it. \`to\` is the \`id\` of the pad this one leads to: always the next \`id\` in the array, and the first one after the last. It is nil when there is only one pad, which does nothing.
- \`state.arena\`: \`{ half_size }\`. The arena spans -half_size..half_size on both axes and bots cannot leave it. Up to four bots fight, each starting at the middle of a different edge.
- \`state.rules\`: \`{ ticks_per_sec, max_ticks, walk_speed, attack_range, attack_damage, attack_cooldown, attack_windup, knockback, max_hp, max_stamina, stamina_regen, attack_stamina_cost, dash_stamina_cost, dash_speed, shield_stamina_cost, shield_block_stamina_cost, shield_lockout, shield_speed, teleporter_radius }\`. \`walk_speed\`, \`dash_speed\` and \`shield_speed\` are in units per second.

The last bot standing wins, whether the others ran out of hp or were thrown out. If the match reaches \`max_ticks\`, the bot with the most hp wins.

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
