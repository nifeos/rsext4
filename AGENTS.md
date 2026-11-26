# Working in this repository

This repository is a fork of `rsext4` from rcore-os/tgoskits (`fs/rsext4`). Read
[CONTRIBUTING.md](CONTRIBUTING.md) for what the gates ask of a change and [CHANGES.md](CHANGES.md)
for every file the fork changes and where each fix stands upstream. This file holds the two rules
that keep the fork and upstream in step: fork work comes first, and upstream `dev` is merged in
as a routine step.

## Fork work comes first

- A fix lands here first, through a pull request of this repository and its gates. Only then is it
  carried to tgoskits `dev`, as an issue and a pull request of its own, written to upstream's
  conventions.
- An answer to an upstream review lands here first too: change the fork, pass the gates, merge,
  then update the upstream pull request to match.
- The upstream pull request is a copy of the fork's change, never the other way round. CHANGES.md
  names the upstream issue and pull request for each fix and says whether upstream has merged it.
- Never push to rcore-os/tgoskits. An upstream pull request comes from a fork of tgoskits.

## Merging upstream `dev`

The fork's history is a subtree split of `fs/rsext4`, so upstream's commits are merged in as they
are, never rewritten. `.github/fork-upstream.toml` names the upstream commit the fork tracks; the
sync moves it forward. The `upstream sync` workflow (`.github/workflows/upstream-sync.yml`) does
steps 1 and 2 every week and opens a draft pull request when the merge is clean, or an issue
naming the conflicting files when it is not. Steps 3 to 6 are always done by hand.

1. Split upstream. In a clone of rcore-os/tgoskits with `origin` pointing at it:

   ```sh
   git fetch origin dev
   git subtree split --prefix=fs/rsext4 origin/dev -b rsext4-upstream
   git merge-base --is-ancestor 359e66d0136c5b6acf44f0f3aba070b5a3457998 rsext4-upstream
   ```

   The split is deterministic: it reproduces the commits this fork already holds, starting from
   `359e66d`, the oldest. If the last command fails, the split is not the fork's history; stop and
   find out why before merging anything.

2. Merge it. In this repository, on a new branch from `origin/main`:

   ```sh
   git fetch <path to the tgoskits clone> rsext4-upstream
   git merge --no-ff FETCH_HEAD -m "Merge upstream tgoskits dev at <upstream commit> (fs/rsext4)"
   ```

   Here `<upstream commit>` is the tgoskits `dev` commit that was split, not the split commit.
   Never rebase, squash or cherry-pick upstream's commits: the next split must find them here.

3. Resolve conflicts by this rule:
   - A fix of ours that upstream has merged takes upstream's version, unless this fork holds a
     later fix to the same lines; then the later fix stays and its CHANGES.md row says so.
     `gh pr list -R rcore-os/tgoskits --author <our account> --state merged` lists them.
   - A fix upstream has not merged, and a change listed under "Changes only this fork needs" in
     CHANGES.md, stay, applied to upstream's new code: among them the literal `Cargo.toml` keys and
     version, the Miri gates, the falsification patches, the removed workflows and the added
     license file.
   - Anything else takes upstream's version.
   - A new upstream test module gets the Miri gate the other test modules carry, unless it reaches
     unsafe code.
   - A new dependency in upstream's `Cargo.toml` is added here too, and holds the pull request
     until a maintainer approves it, as CONTRIBUTING.md says.

4. Update the records in the same pull request:
   - `.github/fork-upstream.toml`: `commit` is the `dev` commit that was split.
   - CHANGES.md: the opening paragraph names that commit. A fix upstream has merged moves to say
     so, with the upstream commit; once upstream releases it, the fork stops carrying it. A row
     whose files upstream renamed names the new files.
   - The exemption files under `.github/` (file length, function length, acronyms, coverage) list
     upstream code at its measured size; an entry for code upstream changed is updated to the new
     measure, and one for code that is gone is removed. An entry whose file also holds code of the
     fork names that code in its reason. New upstream code under the coverage floor is exempted at
     its measured figure in a commit of its own, and the pull request says so.

5. Run the gates before opening the pull request:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features
   cargo test
   cargo test --no-default-features
   cargo build --target aarch64-unknown-none-softfloat
   cargo build --target riscv64imac-unknown-none-elf
   cargo build --target x86_64-unknown-none
   cargo kani
   cargo +nightly miri test
   cargo +<rust-version> test --all-features
   ```

   An upstream change that breaks a gate is reported in the pull request, with the change that
   fixes it in a commit of its own; a gate is never weakened to let the merge through.

6. Open the pull request against `main`. Its description names the upstream commit, the
   conflicts and how each was resolved, and the gate results.
