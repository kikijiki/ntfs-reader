// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! The driver of the property tests (`mft::structured_tests`, `usn::structured_tests`).
//!
//! Each test describes an input with `#[derive(Arbitrary)]` types and checks invariants on what
//! the crate makes of it. [`arbtest`] feeds the description from seeded random bytes, over and
//! over until a time budget runs out, on stable Rust and under plain `cargo test`.
//!
//! - The budget is [`DEFAULT_BUDGET_MS`] per test; `ARBTEST_BUDGET_MS=60000` runs a test for a
//!   minute.
//! - A failure prints `arbtest failed! Seed: 0x...`. Replay that one case with
//!   `ARBTEST_SEED=0x... cargo test --features internals --lib <test name>`; a replay also
//!   prints the description it generated. Add `ARBTEST_SEED` only when running a single test:
//!   it applies to every property test the command selects.

use std::fmt::Debug;

use arbitrary::{Arbitrary, Unstructured};

/// How long each property test searches when `ARBTEST_BUDGET_MS` is not set.
const DEFAULT_BUDGET_MS: u64 = 2_000;

/// Runs `property` on random descriptions until the budget is spent or it panics.
pub(crate) fn property(check: impl FnMut(&mut Unstructured<'_>) -> arbitrary::Result<()>) {
    let mut test = arbtest::arbtest(check);
    // An explicit budget would win over the environment variable.
    if std::env::var_os("ARBTEST_BUDGET_MS").is_none() {
        test = test.budget_ms(DEFAULT_BUDGET_MS);
    }
    test.run();
}

/// A list of up to `MAX` items, its length drawn evenly. `Vec`'s own `Arbitrary` flips a coin per
/// item, so lists average about one item and a description rarely holds the several linked records
/// (a file, its parent directory, the root) that most invariants need. Use it with
/// `#[arbitrary(with = list::<Item, MAX>)]`.
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
