# Contributing

On NixOS/Linux, enter the development shell directly or let direnv load it:

```sh
nix develop
# or: direnv allow

cargo xwin build --target x86_64-pc-windows-msvc
cargo xwin check --target i686-pc-windows-msvc
```

The flake includes Rust, both Windows MSVC targets, `cargo-xwin` and the LLVM
linker tools; Windows is still needed to run tests that touch a raw NTFS volume.

On x86_64 Linux, the same shell includes the project's QEMU Windows VM launcher.
Its mutable disks and installation media live outside the checkout, in
`$NTFS_READER_VM_DIR` (default `~/.local/share/windows-vm`):

```sh
ntfs-windows-vm start
ntfs-windows-vm status
ntfs-windows-vm view
ntfs-windows-vm stop
```

Start it without entering the shell with `nix run .#windows-vm -- start`.

Use plain cargo, or install [mise](https://mise.jdx.dev/):

```sh
curl https://mise.run | sh
```

Tasks

On Windows these run Cargo natively; elsewhere they enter the Nix shell and
cross-compile for Windows MSVC.

```sh
mise fix      # Fix format and fixable linting errors
mise check    # Check format and linting issues
mise build    # Build debug
mise release  # Build release
mise test     # Run tests on Windows (with `internals`); compile them for MSVC elsewhere
mise test-32  # Run 32-bit tests on Windows (with `internals`); check all targets elsewhere
mise bench    # Run benchmarks on Windows; compile them elsewhere
mise bench-build # Build the benchmarks without running them
mise bench-linux # Run the synthetic benchmarks natively on Linux
mise check-internals # Lint the `internals` feature and run its unit tests
mise check-linux     # Type-check `internals` on Linux
mise test-linux      # Run the unit and property tests natively on Linux
mise test-linux-long # The same with every property test searching for a minute
mise check-msrv      # Check with the minimum supported Rust version
mise package  # Build and verify the crate package without uploading
```

The integration tests and benchmarks read a real volume, need an elevated shell, and some
write gigabytes and recreate the USN journal. They run only on a volume you name:
disposable (a spare partition or a VHD), never the system drive:

```sh
set NTFS_READER_TEST_VOLUME=T
cargo test --features internals
```

The tests build with the hidden `internals` feature, needed by some of them and the
synthetic benchmarks; it is not part of the crate's API.

Without `NTFS_READER_TEST_VOLUME` they fail immediately. The system drive is refused unless
`NTFS_READER_ALLOW_SYSTEM_DRIVE=1` is also set (for throwaway CI runners). Unit tests need no
volume and no elevation (each module loaded via `#[path]` from `src/tests/<module>.rs`, or its
topic files under `src/tests/<module>/` for a multi-file suite; cross-module scenarios in
`src/tests/scenarios.rs`).

### What the integration tests need from the machine

- **A USN journal on the test volume.** `deleted_journal_tests` panics without one; `journal_tests`
  also need one: `fsutil usn createjournal m=1000 a=100 T:`.
- **Nothing else creating files on the test volume while they run.** The deleted-file tests rely on
  NTFS reusing a freed record for the next file created; anything else making one (another test
  binary, an indexer, an antivirus scan holding a file open) can take the record a test is waiting
  for, or defer a delete. `recoverability_tests` and the reuse tests in `deleted_metadata_tests` are
  most sensitive. Run test binaries one after another (`--test-threads=1` is safest) and keep
  indexing and scanning off the test volume.
- **`cargo test`, not nextest, for `recoverability_tests`, with `--test-threads=1`.** The lock that
  serialises its tests is per process; nextest runs every test in its own process.

Environment variables the tests read:

| Variable | Effect |
| --- | --- |
| `NTFS_READER_TEST_VOLUME` | The drive letter of the disposable volume the tests may write to (required). |
| `NTFS_READER_ALLOW_SYSTEM_DRIVE=1` | Accept the system drive as the test or parity volume (throwaway machines only). |
| `NTFS_READER_PARITY_VOLUME` | The volume the read-only checks use (the Win32 parity test, the WOF stream test, the smoke check of the deleted-file API on a real volume). Tests that need it skip when it is unset. |
| `NTFS_READER_ALLOW_FSUTIL=1` | Lets a test change something outside the test volume that outlives it: `fsutil behavior set DisableDeleteNotify` (system-wide, persists across reboots; a guard restores it and leaves a marker file in `%TEMP%` in case the process is killed) and `EncryptFileW`, which creates an EFS certificate and key in your user profile if you have none. Unset, those tests skip (and pass). |
| `NTFS_READER_REQUIRE_ALL=1` | A test that cannot check what it is for (a missing fixture, a state it could not reach) fails instead of skipping. |
| `NTFS_READER_SKIP_LOG` | A file every skip is appended to. libtest hides a passing test's output, so this is how a run reports what was not checked; a line starting with `[env]` is a skip caused by the environment (for example a disk that never receives TRIM), which `NTFS_READER_REQUIRE_ALL` does not turn into a failure. |

A default `cargo test` with no optional variables set is meant to be safe: tests that change system
settings skip (and pass), as do ones needing a fixture or a parity volume, each printing
`SKIPPED: <test>: <reason>` to stderr (`--nocapture` shows it). Only tests needing the test volume
fail at once when `NTFS_READER_TEST_VOLUME` is unset. The maintainer's VM runs everything with
`NTFS_READER_ALLOW_FSUTIL=1` and `NTFS_READER_REQUIRE_ALL=1`; GitHub Actions runs with neither and
checks the skip count matches expectations.

## Property tests and how long they search

The unit tests build synthetic MFT records. The property tests (`src/tests/mft/structured.rs`,
`src/tests/usn/structured.rs`, `src/tests/stream.rs` and `src/tests/bitmap.rs`: nine in all) search
random descriptions of an MFT, stream, bitmap or USN buffer for a case where the crate and a model
built from the description disagree. They run natively on Linux (`mise test-linux`) and as part of
`cargo test --features internals` on Windows. Each searches for a time budget: 2 seconds by default,
10 seconds for the three with the richest descriptions (the deleted-file models and the MFT loader),
or whatever `ARBTEST_BUDGET_MS` sets.

Two real divergences between the crate and its model turned up only after about 20 seconds of
search. Before a release, and after changing a property test or the deleted-file code, run

```sh
mise test-linux-long   # every property test searches for a minute
```

A failure prints `arbtest failed! Seed: 0x...`; `ARBTEST_SEED=0x... cargo test --features internals --lib
<test name>` replays that case and prints the description it generated. CI runs the property tests
with a fresh random seed each time (that is how bugs are found), so a failure's seed is in the
CI log.

Two of the nine, `the_deleted_view_of_a_file_is_its_live_view` and
`the_deleted_view_is_what_the_description_says_for_a_partly_deleted_volume`, also count the shapes
reached (files with extension records, lost extents, wrapped sequence numbers, `<deleted>` paths...)
and fail with "too few ..." if the search was too short, so a green run actually covered the cases
it claims. That failure is about search length, not a bug, and has no seed: search longer
(`ARBTEST_BUDGET_MS=60000`). Counts are not checked when replaying one seed (`ARBTEST_SEED`) or when
`ARBTEST_BUDGET_MS` is under 5000 (a quick look): neither reaches them.

## Win32 parity test and the stress volume

`tests/win32_parity_tests.rs` compares the crate with Win32 for every file on a volume: hard links
(`FindFirstFileNameW`), data streams (`FindFirstStreamW`), the file id of `FileInfo.path` reopened,
size, attributes and the four timestamps (`GetFileInformationByHandleEx`), `is_directory`, and every
directory's children. It only reads, works on any NTFS volume, and is `#[ignore]`d: runs only when asked:

```sh
set NTFS_READER_PARITY_VOLUME=S
cargo test --features internals --test win32_parity_tests -- --ignored --nocapture
```

Without `NTFS_READER_PARITY_VOLUME` it fails at once; the system drive needs
`NTFS_READER_ALLOW_SYSTEM_DRIVE=1`. It prints counts per check and the first mismatches, failing on any.
`NTFS_READER_PARITY_STRIDE=n` checks only every n-th file.

The benches (`mft_benchmark`, `cache_memory`) take their volume from `NTFS_READER_TEST_VOLUME` like the
tests, so a million-file volume gives numbers at that scale.

On the maintainer's Linux box, the QEMU VM has a second, thin test disk for this (`ntfs-stress.qcow2`,
2 TB virtual, created by `ntfs-windows-vm start`; `NTFS_READER_STRESS_DISK` moves it). A generator
script builds a fixture of about a million files plus the odd cases (huge sparse and allocated files,
1024 hard links, thousands of streams, odd names, extreme timestamps, a replay of the tree from issue
#1) on it. The scripts and how to run the stress stage live in the maintainer's notes, not this
repository.
