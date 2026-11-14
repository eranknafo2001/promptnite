-- Reference bot: go for the nearest enemy, dashing while it is far away, and
-- punch when in range.
--
-- The game calls tick(state) once per simulation step and expects one action:
--   { action = "walk", dx = <number>, dy = <number> }   direction, any length
--   { action = "dash", dx = <number>, dy = <number> }   a faster step, costs stamina
--   { action = "attack" }                               hit nearest enemy in range, costs stamina
--   { action = "idle" }                                 (or return nil)
--
-- Globals persist between ticks, so a script may keep its own memory.

function tick(state)
  local target = nil
  for _, enemy in ipairs(state.enemies) do
    if target == nil or enemy.dist < target.dist then
      target = enemy
    end
  end

  if target == nil then
    return { action = "idle" }
  end

  if target.dist <= state.rules.attack_range and state.me.cooldown == 0 then
    return { action = "attack" }
  end

  local step = { action = "walk", dx = target.x - state.me.x, dy = target.y - state.me.y }
  -- Dash to close a long gap, but arrive with stamina left to fight on.
  if target.dist > 3 * state.rules.attack_range and state.me.stamina >= 60 then
    step.action = "dash"
  end
  return step
end
