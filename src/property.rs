// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Driver for the property tests (`mft::tests::structured`, `usn::tests::structured`, the stream
//! tests).
//!
//! Each test describes an input with `#[derive(Arbitrary)]` types and checks invariants on what
//! the crate makes of it. [`arbtest`] feeds the description from seeded random bytes until a time
//! budget runs out, on stable Rust under plain `cargo test`.
//!
//! - Budget: [`DEFAULT_BUDGET_MS`] per test, [`HEAVY_BUDGET_MS`] for the three heaviest (see
//!   [`heavy_property`]). `ARBTEST_BUDGET_MS=60000` runs a test for a minute; `mise run
//!   test-linux-long` does that for every property test.
//! - Two tests also count the shapes they reached and fail if the search was too short to reach
//!   them (see [`coverage`]). Skipped when a seed is replayed (`ARBTEST_SEED`), the budget is a
//!   quick look under [`GATE_MIN_BUDGET_MS`], or `ARBTEST_COVERAGE=off` (CI: how far a timed
//!   search gets depends on the runner's speed, and CI must not fail by chance).
//! - A failure prints `arbtest failed! Seed: 0x...`. Replay it with `ARBTEST_SEED=0x... cargo test
//!   --features internals --lib <test name>`, which also prints the generated description. Set
//!   `ARBTEST_SEED` only for a single test: it applies to every property test the command selects.

use std::fmt::Debug;

use arbitrary::{Arbitrary, Unstructured};

/// How long each property test searches when `ARBTEST_BUDGET_MS` is not set.
const DEFAULT_BUDGET_MS: u64 = 2_000;

/// How long the heaviest property tests search when `ARBTEST_BUDGET_MS` is not set. Two real
/// divergences between the crate and its model surfaced only after about 20 seconds of search, so
/// the tests with the richest descriptions get more than the default.
pub(crate) const HEAVY_BUDGET_MS: u64 = 10_000;

/// The shortest search, in milliseconds, whose coverage counts are still checked (see
/// [`coverage`]).
pub(crate) const GATE_MIN_BUDGET_MS: u64 = 5_000;

/// Runs `property` on random descriptions until the budget is spent or it panics.
pub(crate) fn property(check: impl FnMut(&mut Unstructured<'_>) -> arbitrary::Result<()>) {
    run(DEFAULT_BUDGET_MS, check);
}

/// [`property`] for the tests with the richest descriptions: [`HEAVY_BUDGET_MS`], unless
/// `ARBTEST_BUDGET_MS` overrides it (`=1000` for a quick look, `=60000` for the long run of `mise
/// run test-linux-long`).
pub(crate) fn heavy_property(check: impl FnMut(&mut Unstructured<'_>) -> arbitrary::Result<()>) {
    run(HEAVY_BUDGET_MS, check);
}

fn run(default_budget_ms: u64, check: impl FnMut(&mut Unstructured<'_>) -> arbitrary::Result<()>) {
    let mut test = arbtest::arbtest(check);
    // budget_ms() here would override arbtest's own use of the env var.
    if std::env::var_os("ARBTEST_BUDGET_MS").is_none() {
        test = test.budget_ms(default_budget_ms);
    }
    test.run();
}

/// Whether coverage counts are checked, given `ARBTEST_COVERAGE`, `ARBTEST_SEED` and
/// `ARBTEST_BUDGET_MS`: not when switched off (`ARBTEST_COVERAGE=off`), not when a seed is set
/// (arbtest then runs that one case once), and not when the budget is a quick look under
/// [`GATE_MIN_BUDGET_MS`]. A budget that fails to parse is arbtest's own error to report, so it
/// does not disable the checks.
fn gates_apply(coverage: Option<&str>, seed: Option<&str>, budget_ms: Option<&str>) -> bool {
    if coverage.is_some_and(|value| value.trim() == "off") || seed.is_some() {
        return false;
    }
    match budget_ms.and_then(|ms| ms.trim().parse::<u64>().ok()) {
        Some(ms) => ms >= GATE_MIN_BUDGET_MS,
        None => true,
    }
}

/// Coverage gate for a property test: the search must reach more than `more_than` cases of the
/// shape `what`, or a green run proves nothing about it. Skipped when it cannot mean anything (see
/// [`gates_apply`]). A failure here has no seed to replay; the message says the search was too
/// short and gives the command to search longer.
#[track_caller]
pub(crate) fn coverage(what: &str, reached: u64, more_than: u64) {
    let switch = std::env::var("ARBTEST_COVERAGE").ok();
    let seed = std::env::var("ARBTEST_SEED").ok();
    let budget = std::env::var("ARBTEST_BUDGET_MS").ok();
    if !gates_apply(switch.as_deref(), seed.as_deref(), budget.as_deref()) {
        return;
    }
    assert!(
        reached > more_than,
        "too few {what}: {reached} (more than {more_than} needed). The search was too short to reach them, \
         which is not a bug in the crate: search longer with `ARBTEST_BUDGET_MS=60000 cargo test \
         --features internals --lib` (`mise run test-linux-long`). A failing case, unlike this, prints \
         `arbtest failed! Seed: 0x...`; replay it with `ARBTEST_SEED=0x... cargo test --features \
         internals --lib <test name>`"
    );
}

/// A list of up to `MAX` items, its length drawn evenly. `Vec`'s own `Arbitrary` flips a coin per
/// item, so lists average about one item, too few for the several linked records (a file, its
/// parent directory, the root) most invariants need. Use with `#[arbitrary(with = list::<Item,
/// MAX>)]`.
pub(crate) fn list<'a, T: Arbitrary<'a>, const MAX: usize>(
    u: &mut Unstructured<'a>,
) -> arbitrary::Result<Vec<T>> {
    let length = u.int_in_range(0..=MAX)?;
    (0..length).map(|_| T::arbitrary(u)).collect()
}

/// Prints `description` when replaying a seed, so the failing input can be read.
pub(crate) fn show_on_replay(description: &impl Debug) {
    if std::env::var_os("ARBTEST_SEED").is_some() {
        eprintln!("{description:#?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gates_apply_to_a_normal_search() {
        assert!(gates_apply(None, None, None));
        assert!(gates_apply(None, None, Some("60000")));
        assert!(gates_apply(None, None, Some(" 5000 ")));
        assert!(gates_apply(Some("on"), None, None));
    }

    #[test]
    fn the_gates_do_not_apply_when_switched_off() {
        assert!(!gates_apply(Some("off"), None, None));
        assert!(!gates_apply(Some(" off "), None, Some("60000")));
    }

    #[test]
    fn the_gates_do_not_apply_to_a_replayed_seed() {
        assert!(!gates_apply(None, Some("0x1234"), None));
        // Whatever the budget: one seed is one case.
        assert!(!gates_apply(None, Some("0x1234"), Some("60000")));
    }

    #[test]
    fn the_gates_do_not_apply_to_a_quick_look() {
        assert!(!gates_apply(None, None, Some("1000")));
        assert!(!gates_apply(None, None, Some("4999")));
        assert!(!gates_apply(None, None, Some("0")));
    }

    #[test]
    fn a_budget_that_does_not_parse_is_left_to_arbtest() {
        assert!(gates_apply(None, None, Some("soon")));
    }

    #[test]
    fn a_gate_that_is_not_met_says_how_to_search_longer_and_how_to_replay() {
        // This process's own env vars decide whether the gate applies; build the message the same
        // way `coverage` does, for a run with none of its variables set.
        if !gates_apply(
            std::env::var("ARBTEST_COVERAGE").ok().as_deref(),
            std::env::var("ARBTEST_SEED").ok().as_deref(),
            std::env::var("ARBTEST_BUDGET_MS").ok().as_deref(),
        ) {
            return;
        }
        let failure = std::panic::catch_unwind(|| coverage("widgets", 3, 10)).unwrap_err();
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(message.contains("too few widgets: 3"), "{message}");
        assert!(message.contains("ARBTEST_BUDGET_MS=60000"), "{message}");
        assert!(message.contains("ARBTEST_SEED=0x"), "{message}");
        coverage("widgets", 11, 10);
    }
}
