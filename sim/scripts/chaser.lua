-- Reference bot: presses while it has the stamina for it, gets out of the
-- way of a swing it sees coming (or blocks it), and swings back while the
-- enemy is still recovering.
--
-- The game calls tick(state) once per simulation step and expects one action:
--   { action = "walk", dx = <number>, dy = <number> }   direction, any length
--   { action = "dash", dx = <number>, dy = <number> }   a faster step, costs stamina
--   { action = "shield", dx = <number>, dy = <number> } block every hit this tick, costs stamina;
--                                                       locked out for a while after a block;
--                                                       the direction is optional, a slow step
--   { action = "attack" }                               start winding up a swing, costs stamina;
--                                                       it lands a few ticks later on the nearest
--                                                       enemy in range, and throws it back
--   { action = "idle" }                                 (or return nil)
--
-- Globals persist between ticks, so a script may keep its own memory.

-- A step away from (dx, dy), but never into the railing, where the next hit
-- would throw us out: slide along it instead.
local function away(state, action, dx, dy)
  local me = state.me
  local safe = state.arena.half_size - 1 - state.rules.knockback
  if math.abs(me.x) > safe and me.x * dx < 0 then dx = 0 end
  if math.abs(me.y) > safe and me.y * dy < 0 then dy = 0 end
  return { action = action, dx = -dx, dy = -dy }
end

function tick(state)
  local me, rules = state.me, state.rules
  local target = nil
  for _, enemy in ipairs(state.enemies) do
    if target == nil or enemy.dist < target.dist then
      target = enemy
    end
  end

  if target == nil then
    return { action = "idle" }
  end

  local dx, dy = target.x - me.x, target.y - me.y
  local toward = { action = "walk", dx = dx, dy = dy }
  local tps = rules.ticks_per_sec

  -- Our own swing is on its way: stay on the target so it lands.
  if me.winding > 0 then
    return toward
  end

  -- Its swing is on its way and could reach us.
  if target.winding > 0 and target.dist <= target.attack_range + 1 then
    if target.winding == 1 then
      -- It lands this tick, and it walks after us while we step back.
      local reach = target.attack_range + target.walk_speed / tps
      if target.dist + rules.walk_speed / tps > reach then
        return away(state, "walk", dx, dy)
      end
      if me.shield_cooldown == 0
        and me.stamina >= rules.shield_stamina_cost + rules.shield_block_stamina_cost then
        return { action = "shield" }
      end
      if me.stamina >= rules.dash_stamina_cost and target.dist + rules.dash_speed / tps > reach then
        return away(state, "dash", dx, dy)
      end
    end
    return away(state, "walk", dx, dy)
  end

  local in_range = target.dist <= rules.attack_range - 0.05
  if me.cooldown == 0 and me.stamina >= rules.attack_stamina_cost then
    -- It has just swung and cannot break our wind-up: this one is safe.
    if in_range and target.cooldown > 0 then
      return { action = "attack" }
    end
    -- Press while there is stamina to defend afterwards. Not on the first
    -- tick every time, so two of us do not just swing in step.
    if me.stamina >= 50 then
      if in_range then
        return math.random() < 0.5 and { action = "attack" } or toward
      end
      -- Dash to close a long gap, but arrive with stamina left to fight on.
      if target.dist > 3 * rules.attack_range and me.stamina >= 80 then
        toward.action = "dash"
      end
      return toward
    end
  end

  -- Out of breath or on cooldown: stay just out of its reach and recover,
  -- circling so it cannot walk us back against the railing.
  local hold = target.attack_range + 0.5
  if target.dist < hold then
    return away(state, "walk", dx, dy)
  end
  if target.dist > hold + 1 then
    return toward
  end
  return { action = "walk", dx = -dy, dy = dx }
end
