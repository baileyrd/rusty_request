//! A tiny non-cryptographic random source, used only where the goal is
//! avoiding collisions/lockstep -- retry-backoff jitter and multipart
//! boundary generation -- never anything security-sensitive. Built from
//! `std`'s own `RandomState` (already randomly seeded from OS randomness
//! per the stdlib docs) perturbed with the current time, rather than
//! pulling in a `rand` crate or hand-rolling a CSPRNG. A real CSPRNG
//! belongs nowhere near this MVP -- same reasoning as the TLS gap in the
//! README.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn next_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    hasher.write_u128(nanos);
    hasher.finish()
}
