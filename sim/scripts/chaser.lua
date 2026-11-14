-- Reference bot: walk toward the nearest enemy and punch when in range.
--
-- The game calls tick(state) once per simulation step and expects one action:
--   { action = "walk", dx = <number>, dy = <number> }   direction, any length
--   { action = "attack" }                               hit nearest enemy in range
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

  return { action = "walk", dx = target.x - state.me.x, dy = target.y - state.me.y }
end
