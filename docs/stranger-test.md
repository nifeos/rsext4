# The stranger cadence

The release's whole claim is that a stranger, given this repository and nothing else, can build
the crate and run its proofs from the README alone. A real agent runs that check on a
cadence, and the weekly `stranger cadence` job fails when the
last run is past 28 days.

What a run is:

1. Clone the repository fresh.
2. Follow the README and nothing else: build, test, and run the proofs.
   The README must name the build and test commands (with any feature a test target needs), the
   bare-metal build with `rustup target add`, the Kani install before the first `cargo kani`, how
   long the proofs take and how to run one harness, what a passing run prints that looks alarming,
   how to run and revert one falsification patch, and how to check the minimum Rust version.
   Each one it lacks is a finding.
3. Note every place the README lied, omitted, or assumed. Those are findings against the README,
   not against the stranger.
4. Record the date on the line below.

Last run: 2026-11-25

## What the last run did

A fresh clone and the README, on a machine that already had Rust, the three bare-metal targets,
the 1.94 toolchain, Kani 0.68.0 and e2fsprogs 1.47.2. It followed "Building and testing", "What is
proven" and "Minimum Rust version" command by command.

- `cargo build` and `cargo test --all-features` pass: 629 tests, 0 failed. `cargo build --lib`
  passes for `aarch64-unknown-none-softfloat`, `riscv64imac-unknown-none-elf` and
  `x86_64-unknown-none`.
- `cargo kani` ends with `Complete - 8 successfully verified harnesses, 0 failures, 8 total.`
  after 3 minutes 36 seconds. One harness by its full path takes about 6 seconds.
- With the `directory_entry_parse_is_total` falsification applied, that harness fails with
  `VERIFICATION:- FAILED`, and the reversed patch leaves `git status` clean.
- At Rust 1.94, the stated minimum, the tests and the three bare-metal builds pass.

The README then lacked the Kani install, how long the proofs take, how to run one harness, what a
passing proof run prints that looks alarming, how to apply and revert a falsification patch, the
`rustup target add` for the minimum toolchain, where e2fsprogs comes from, and the `Cell` import
in its example. This version adds each of them.

Result: fail. This version of the README corrects every finding.
