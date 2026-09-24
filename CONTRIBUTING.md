# Contributing

On NixOS/Linux, enter the development shell directly or let direnv load it:

```sh
nix develop
# or: direnv allow

cargo xwin build --target x86_64-pc-windows-msvc
cargo xwin check --target i686-pc-windows-msvc
```

The flake includes Rust, both Windows MSVC Rust targets, `cargo-xwin`, and the
LLVM linker tools. Windows is still required to execute tests that access a raw
NTFS volume.

On x86_64 Linux, the same development shell includes the project's QEMU Windows
VM launcher. Its mutable disks and installation media live outside the checkout
in `$NTFS_READER_VM_DIR` (by default `~/.local/share/windows-vm`):

```sh
ntfs-windows-vm start
ntfs-windows-vm status
ntfs-windows-vm view
ntfs-windows-vm stop
```

You can also start it without entering the shell with
`nix run .#windows-vm -- start`.

You can use plain cargo or install [mise](https://mise.jdx.dev/):

```sh
curl https://mise.run | sh
```

Tasks

On Windows these run Cargo natively. On other platforms they enter the Nix
development shell and cross-compile for Windows MSVC.

```sh
mise fix      # Fix format and fixable linting errors
mise check    # Check format and linting issues
mise build    # Build debug
mise release  # Build release
mise test     # Run tests on Windows (with `internals`); compile them for MSVC elsewhere
mise test-32  # Run 32-bit tests on Windows (with `internals`); check all targets elsewhere
mise bench    # Run benchmarks on Windows; compile them elsewhere
mise check-internals # Lint the `internals` feature and run its unit tests
mise check-linux     # Type-check `internals` on Linux
mise test-linux      # Run the unit and property tests natively on Linux
mise check-msrv      # Check with the minimum supported Rust version
mise publish -n # Verify the crate package without uploading
```

The integration tests and benchmarks read a real volume, need an elevated shell, and some
of them write gigabytes and delete and recreate the volume's USN journal. They therefore
run only on a volume you name, which must be a disposable NTFS volume (a spare partition
or a VHD) and not the system drive:

```sh
set NTFS_READER_TEST_VOLUME=T
cargo test --features internals
```

The tests build with the hidden `internals` feature, which some of them (and the synthetic
benchmarks) need; it is not part of the crate's API.

Without `NTFS_READER_TEST_VOLUME` they fail immediately. The system drive is refused unless
`NTFS_READER_ALLOW_SYSTEM_DRIVE=1` is also set, which is meant for throwaway CI runners.

## Win32 parity test and the stress volume

`tests/win32_parity_tests.rs` compares the crate with Win32 for every file of a volume: hard links
(`FindFirstFileNameW`), data streams (`FindFirstStreamW`), the file id of `FileInfo.path` reopened, size,
attributes and the four timestamps (`GetFileInformationByHandleEx`), `is_directory`, and every directory's
children. It only reads, works on any NTFS volume, and is `#[ignore]`d, so it runs only when asked:

```sh
set NTFS_READER_PARITY_VOLUME=S
cargo test --features internals --test win32_parity_tests -- --ignored --nocapture
```

Without `NTFS_READER_PARITY_VOLUME` it fails at once; the system drive needs
`NTFS_READER_ALLOW_SYSTEM_DRIVE=1`. It prints counts per check and the first mismatches, and fails on any.
`NTFS_READER_PARITY_STRIDE=n` checks every n-th file only.

The benches (`mft_benchmark`, `cache_memory`) take their volume from `NTFS_READER_TEST_VOLUME` like the
tests, so on a volume with a million files they give the numbers at that scale.

On the maintainer's Linux box the QEMU VM has a second, thin test disk for this (`ntfs-stress.qcow2`, 2 TB
virtual, created by `ntfs-windows-vm start`; `NTFS_READER_STRESS_DISK` moves it). A generator script builds
a fixture of about a million files and the odd cases (huge sparse and allocated files, 1024 hard links,
thousands of streams, odd names, extreme timestamps, a replay of the tree from issue #1) on it. The scripts
and how to run the stress stage are in the maintainer's notes, not in this repository.
