//! Hostile scripts: each of these once stalled or crashed a match.

use promptnite_sim::{BotSpec, EXAMPLE_SCRIPT, LuaBot, rules, run_match, validate_script};

const MATCH_TICKS: u32 = rules::MAX_MATCH_SECS * rules::TICKS_PER_SEC;

/// A full room running the same script for the longest match there is.
fn full_match(script: &str) -> promptnite_sim::MatchOutcome {
    let specs: Vec<BotSpec> = (0..rules::MAX_BOTS as u32)
        .map(|edge| BotSpec {
            id: edge + 1,
            edge,
            script: script.to_string(),
        })
        .collect();
    run_match(&specs, 7, MATCH_TICKS)
}

#[test]
fn concatenation_is_refused() {
    // Doubling a string is the one way to allocate a gigabyte in a handful
    // of instructions, between two looks at the heap.
    let error = validate_script("local s = 'x' function tick() s = s .. s end").unwrap_err();
    assert!(error.contains("concatenation"), "{error}");

    // Nested functions are checked too.
    let nested = "function tick() local f = function(a) return a .. a end end";
    assert!(validate_script(nested).is_err());
}

#[test]
fn long_string_literals_are_refused() {
    let long = "x".repeat(rules::MAX_STRING_LITERAL_BYTES + 1);
    let error = validate_script(&format!("local s = '{long}' function tick() end")).unwrap_err();
    assert!(error.contains("string literals"), "{error}");

    let short = "x".repeat(rules::MAX_STRING_LITERAL_BYTES);
    validate_script(&format!("local s = '{short}' function tick() end")).unwrap();
}

#[test]
fn hoarding_memory_while_loading_is_refused() {
    let script = "local t = {} for i = 1, 1e9 do t[i] = { i } end function tick() end";
    assert!(validate_script(script).is_err());
}

#[test]
fn a_script_cannot_outgrow_the_heap_cap() {
    // Fill a table for ever. Whatever stops it (the cap, or fuel that costs
    // more and more), the heap stays small and the match ends.
    let script = "local t = {} function tick() while true do t[#t + 1] = { 1, 2, 3 } end end";
    let mut bot = LuaBot::new(script, 1).unwrap();
    let view = promptnite_sim::lua::BotView {
        tick: 1,
        max_ticks: MATCH_TICKS,
        me: promptnite_sim::lua::Seen {
            id: 1,
            x: 0.0,
            y: 0.0,
            hp: 100,
            dist: 0.0,
        },
        cooldown: 0,
        enemies: &[],
    };
    for _ in 0..MATCH_TICKS {
        if bot.out_of_memory() {
            break;
        }
        assert!(bot.tick(&view).is_err());
        // Garbage is counted until the next collection, hence the slack.
        assert!(bot.memory() < 2 * rules::MAX_LUA_MEMORY_BYTES);
    }
}

#[test]
fn hostile_scripts_cannot_stall_a_match() {
    // Each of these spends as much real time per instruction as it can.
    let scripts = [
        "function tick() while true do end end",
        // One instruction, thousands of values.
        "local t = {} for i = 1, 5000 do t[i] = i end
         function tick() while true do local n = select('#', table.unpack(t)) end end",
        "local t = {} for i = 1, 3000 do t[i] = i end
         function tick() while true do t = table.pack(table.unpack(t)) end end",
        "local function f() return 1 + f() end function tick() f() end",
        "local t = {} function tick() while true do t[#t + 1] = function() return t end end end",
    ];
    for script in scripts {
        validate_script(script).unwrap();
        let started = std::time::Instant::now();
        let outcome = full_match(script);
        // Measured at well under half a second; the bound is loose so that a
        // slow machine does not fail it, and tight enough to catch a stall.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{script} took {:?}",
            started.elapsed()
        );
        assert_eq!(outcome.frames.len() as u32, MATCH_TICKS + 1);
        assert!(!outcome.faults.is_empty());
    }
}

#[test]
fn the_example_bot_fits_the_limits() {
    validate_script(EXAMPLE_SCRIPT).unwrap();
    assert!(full_match(EXAMPLE_SCRIPT).faults.is_empty());
}
