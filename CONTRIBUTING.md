# Contributing

On NixOS/Linux, enter the development shell directly or let direnv load it:

```sh
nix develop
# or: direnv allow

cargo xwin build --target x86_64-pc-windows-msvc
cargo xwin check --target i686-pc-windows-msvc
```

The flake includes Rust, both Windows MSVC targets, `cargo-xwin` and the LLVM linker tools.
Tests that touch a raw NTFS volume still need Windows.

On x86_64 Linux, the same shell includes the project's QEMU Windows VM launcher. Its disks and
installation media live outside the checkout, in `$NTFS_READER_VM_DIR` (default
`~/.local/share/windows-vm`):

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

Tasks:

On Windows these run Cargo natively. Elsewhere they enter the Nix shell and cross-compile for
Windows MSVC.

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

The integration tests and benchmarks read a real volume and need an elevated shell. Some write
gigabytes and recreate the USN journal. They run only on a volume you name, which must be
disposable (a spare partition or a VHD) and never the system drive:

```sh
set NTFS_READER_TEST_VOLUME=T
cargo test --features internals
```

The tests build with the hidden `internals` feature, which some of them and the synthetic
benchmarks need. It is not part of the crate's API.

Without `NTFS_READER_TEST_VOLUME` they fail immediately. The system drive is refused unless
`NTFS_READER_ALLOW_SYSTEM_DRIVE=1` is also set (meant for throwaway CI runners). Unit tests need no
volume and no elevation. Each module is loaded via `#[path]` from `src/tests/<module>.rs`, or from
its topic files under `src/tests/<module>/` for a multi-file suite, and cross-module scenarios are
in `src/tests/scenarios.rs`.

### What the integration tests need from the machine

The test volume needs a USN journal. `deleted_journal_tests` panics without one, and
`journal_tests` need one too: `fsutil usn createjournal m=1000 a=100 T:`.

Nothing else should create files on the test volume while the tests run.

- The deleted-file tests rely on NTFS reusing a freed record for the next file created.
- Another test binary, an indexer, or an antivirus scan holding a file open can take that record,
  or delay a delete.
- `recoverability_tests` and the reuse tests in `deleted_metadata_tests` are the most sensitive.
- Run test binaries one after another (`--test-threads=1` is safest) and keep indexing and scanning
  off the test volume.

Run `recoverability_tests` with `cargo test --test-threads=1`, not nextest. The lock that
serializes its tests is per process, and nextest runs every test in its own process.

Environment variables the tests read:

| Variable | Effect |
| --- | --- |
| `NTFS_READER_TEST_VOLUME` | The drive letter of the disposable volume the tests may write to (required). |
| `NTFS_READER_ALLOW_SYSTEM_DRIVE=1` | Accept the system drive as the test or parity volume (throwaway machines only). |
| `NTFS_READER_PARITY_VOLUME` | The volume the read-only checks use (the Win32 parity test, the WOF stream test, the smoke check of the deleted-file API on a real volume). Tests that need it skip when it is unset. |
| `NTFS_READER_ALLOW_FSUTIL=1` | Lets a test change something outside the test volume that outlives it: `fsutil behavior set DisableDeleteNotify` (system-wide, persists across reboots; a guard restores it and leaves a marker file in `%TEMP%` in case the process is killed) and `EncryptFileW`, which creates an EFS certificate and key in your user profile if you have none. Unset, those tests skip (and pass). |
| `NTFS_READER_REQUIRE_ALL=1` | A test that cannot check what it is for (a missing fixture, a state it could not reach) fails instead of skipping. |
| `NTFS_READER_SKIP_LOG` | A file every skip is appended to. libtest hides a passing test's output, so this is how a run reports what was not checked; a line starting with `[env]` is a skip caused by the environment (for example a disk that never receives TRIM), which `NTFS_READER_REQUIRE_ALL` does not turn into a failure. |

A default `cargo test` with none of the optional variables set is meant to be safe. Tests that
change system settings skip (and pass), and so do tests that need a fixture or a parity volume.
Each skip prints `SKIPPED: <test>: <reason>` to stderr (`--nocapture` shows it). Only tests that
need the test volume fail at once when `NTFS_READER_TEST_VOLUME` is unset. The maintainer's VM runs
everything with `NTFS_READER_ALLOW_FSUTIL=1` and `NTFS_READER_REQUIRE_ALL=1`. GitHub Actions runs
with neither and checks that the skip count matches expectations.

## Property tests and how long they search

The unit tests build synthetic MFT records. The property tests (`src/tests/mft/structured.rs`,
`src/tests/usn/structured.rs`, `src/tests/stream.rs` and `src/tests/bitmap.rs`, nine in all)
generate random descriptions of an MFT, stream, bitmap or USN buffer and look for a case where the
crate disagrees with a model built from the description. They run natively on Linux
(`mise test-linux`) and as part of `cargo test --features internals` on Windows. Each searches for a
fixed time: 2 seconds by default, 10 seconds for the three with the richest descriptions (the
deleted-file models and the MFT loader), or whatever `ARBTEST_BUDGET_MS` sets.

Two real divergences between the crate and its model only showed up after about 20 seconds of
search. Before a release, and after changing a property test or the deleted-file code, run

```sh
mise test-linux-long   # every property test searches for a minute
```

A failure prints `arbtest failed! Seed: 0x...`. Running
`ARBTEST_SEED=0x... cargo test --features internals --lib <test name>` replays that case and prints
the description it generated. CI runs the property tests with a fresh random seed each time (that is
how bugs are found), so the seed of a failure is in the CI log.

Two of the nine, `the_deleted_view_of_a_file_is_its_live_view` and
`the_deleted_view_is_what_the_description_says_for_a_partly_deleted_volume`, also count the kinds of case
reached (files with extension records, lost extents, wrapped sequence numbers, `<deleted>` paths,
and so on). They fail with "too few ..." if the search was too short, so a green run really covered
the cases it claims to. That failure is about search length, not a bug, and has no seed. Search
longer (`ARBTEST_BUDGET_MS=60000`). The counts aren't checked when replaying one seed
(`ARBTEST_SEED`) or when `ARBTEST_BUDGET_MS` is under 5000 (a quick look), because neither reaches
them. CI sets `ARBTEST_COVERAGE=off`, because how far a timed search gets depends on the runner's
speed and CI must not fail by chance. Run the counts locally, ideally with
`mise run test-linux-long`.

## Win32 parity test and the stress volume

`tests/win32_parity_tests.rs` compares the crate with Win32 for every file on a volume: hard links
(`FindFirstFileNameW`), data streams (`FindFirstStreamW`), the file id of `FileInfo.path` reopened,
size, attributes and the four timestamps (`GetFileInformationByHandleEx`), `is_directory`, and every
directory's children. It only reads, works on any NTFS volume, and is `#[ignore]`d, so it runs only
when asked:

```sh
set NTFS_READER_PARITY_VOLUME=S
cargo test --features internals --test win32_parity_tests -- --ignored --nocapture
```

Without `NTFS_READER_PARITY_VOLUME` it fails at once, and the system drive needs
`NTFS_READER_ALLOW_SYSTEM_DRIVE=1`. It prints counts per check and the first mismatches, and fails on
any. `NTFS_READER_PARITY_STRIDE=n` checks only every n-th file.

The benches (`mft_benchmark`, `cache_memory`) take their volume from `NTFS_READER_TEST_VOLUME`, like
the tests, so a million-file volume gives numbers at that scale.

On the maintainer's Linux machine, the QEMU VM has a second, thin test disk for this
(`ntfs-stress.qcow2`, 2 TB virtual, created by `ntfs-windows-vm start`; `NTFS_READER_STRESS_DISK`
moves it). A generator script fills it with a fixture of about a million files plus the odd cases
(huge sparse and allocated files, 1024 hard links, thousands of streams, odd names, extreme
timestamps, a replay of the tree from issue #1). The scripts, and how to run the stress stage, are
in the maintainer's notes, not in this repository.
