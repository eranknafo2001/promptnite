//! Entropy shims for the module sandbox.
//!
//! A SpacetimeDB module is `wasm32-unknown-unknown` with no OS and no JS, so
//! `getrandom` has nowhere to get random bytes from. Two of the Lua VM's
//! dependencies ask for some anyway while starting up:
//!
//!   * `rand` 0.8 (getrandom 0.2) to seed `math.random`. We immediately
//!     reseed it from the match seed, see `promptnite_sim::lua`.
//!   * `ahash` (getrandom 0.3) for its hash-map keys.
//!
//! Neither needs *secret* randomness here, and reducers must be deterministic
//! anyway, so we hand out a fixed pseudo-random stream. Do not use `getrandom`
//! for anything security-relevant inside this module.
//!
//! getrandom 0.3 only looks for its hook when built with
//! `--cfg getrandom_backend="custom"`; that flag lives in
//! `server/.cargo/config.toml`.

use core::sync::atomic::{AtomicU64, Ordering};

static STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// SplitMix64.
fn next() -> u64 {
    let mut z = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

unsafe fn fill(dest: *mut u8, len: usize) {
    // SAFETY: getrandom guarantees `dest` is valid for `len` writes.
    let bytes = unsafe { core::slice::from_raw_parts_mut(dest, len) };
    for chunk in bytes.chunks_mut(8) {
        chunk.copy_from_slice(&next().to_le_bytes()[..chunk.len()]);
    }
}

/// Hook for getrandom 0.2 (`custom` feature, enabled by the `spacetimedb` crate).
#[unsafe(no_mangle)]
unsafe fn __getrandom_custom(dest: *mut u8, len: usize) -> u32 {
    unsafe { fill(dest, len) };
    0
}

/// Hook for getrandom 0.3 (`getrandom_backend="custom"`).
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    unsafe { fill(dest, len) };
    Ok(())
}
