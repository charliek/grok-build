# Upstream sync runbook

This is the defensive procedure for rebasing `gx/main` onto a new snapshot of
`xai-org/grok-build`'s `main` branch. `main` in this repo is a pristine, fast-forward-only
mirror of upstream; `gx/main` is where the fork's commits live, always rebased on top of
it. Read `CLAUDE.md` first if you haven't — it states the branch model this runbook
depends on.

`scripts/gx/sync-upstream.sh` is a strict-bash implementation of every step below. Prefer
running it; this document explains *why* each step exists and gives the exact commands so
the script's behavior (and any manual recovery) is auditable.

A weekly `upstream-drift.yml` workflow runs a **rebase dry-run** (never pushed) against
the live `xai-org/grok-build` `main` and opens/updates a tracking issue on conflict or a
post-rebase `cargo check` failure — so drift surfaces between real syncs, not only when
you happen to run this runbook.

Rebase drills validate this runbook end-to-end without a real upstream change pending;
drill #1 was run and validated on 2026-08-25, with a corresponding `gx/backup/2026-08-25`
backup ref left in place. Run a drill periodically (e.g. before every release, or when the
runbook itself changes) so the procedure is never exercised for the first time under
real pressure.

## Steps

1. **Refuse a dirty worktree.** Nothing below is safe to run over uncommitted changes.

   ```
   git status --porcelain
   ```

   If this prints anything, stop — commit, stash, or discard first.

2. **Record the old refs**, so every later step (backup, range-diff) has a fixed
   before-picture to compare against.

   ```
   upstream_old=$(git rev-parse upstream/main)
   gx_old=$(git rev-parse gx/main)
   ```

3. **Fetch upstream.**

   ```
   git fetch upstream
   ```

4. **Verify upstream didn't rewrite history.** `upstream_old` must still be an ancestor of
   the freshly-fetched `upstream/main`. If it isn't, upstream force-pushed / rewrote its
   history, and every step from here on assumes a normal fast-forward — do not proceed on
   autopilot.

   ```
   git merge-base --is-ancestor "$upstream_old" upstream/main
   ```

   **Failure fallback:** don't attempt to rebase onto rewritten history mechanically.
   Instead, `git format-patch` the gx-only commit series (`upstream_old..gx_old`) and
   apply it onto the new upstream snapshot by hand, resolving conflicts commit-by-commit
   with full attention. This is a manual, deliberate process — there is no scripted
   recovery for rewritten upstream history.

5. **Create a backup ref** before touching anything, named with today's date so multiple
   drills/syncs don't collide:

   ```
   git branch "gx/backup/$(date +%F)" gx/main
   ```

6. **Enable `rerere`** so any conflict resolution made during this sync is remembered and
   auto-applied if the same conflict recurs later in the same rebase (e.g. after an
   `--abort` and retry):

   ```
   git config rerere.enabled true
   ```

7. **Fast-forward `main` to `upstream/main`.** `main` is upstream-only and ff-only by
   contract — this must never be a merge commit or a rebase.

   ```
   git checkout main
   git merge --ff-only upstream/main
   ```

8. **Rebase `gx/main` onto the now-updated `main`.**

   ```
   git checkout gx/main
   git rebase main
   ```

9. **On any conflict you did not expect, ABORT — do not resolve it blind.**

   ```
   git rebase --abort
   ```

   Every gx commit touches a small, known set of files (see `CLAUDE.md` for where gx code
   lives). A conflict inside gx-owned files with a clear, mechanical resolution is fine to
   resolve and continue. A conflict anywhere else, or one whose correct resolution isn't
   obvious from reading both sides, means stop, abort, and investigate by hand — possibly
   falling back to the `format-patch` path from step 4.

10. **`Cargo.lock`: never blind-regenerate.** Do not run `cargo update` or delete-and-
    regenerate the lockfile speculatively "to fix things." Only touch it in response to a
    *concrete* `cargo` error that names it, and when you do, review the diff — a lockfile
    regenerated to satisfy one dependency bump should not silently drag unrelated
    transitive versions along with it.

11. **Gate: run the targeted test set** — the same set CI runs, so a locally-green sync
    matches CI. Never run the full workspace suite; see `CLAUDE.md` for why the set is
    targeted. All `--locked`, matching CI:

    ```
    cargo check -p xai-grok-pager-bin --locked
    cargo test -p xai-grok-sampling-types --locked
    cargo test -p xai-grok-config --locked
    cargo test -p xai-grok-pager --lib --locked
    cargo test -p xai-grok-update --locked -- \
      --skip install_scripts_allow_custom_https_proxy_url \
      --skip install_scripts_refuse_bad_proxy_url_for_deployment_key
    cargo test -p xai-grok-version --locked
    cargo test -p xai-grok-shell --lib --locked -- leader:: agent::model_providers::tests:: session_compact
    cargo test -p xai-grok-shell --locked --test test_sampling_client
    cargo test -p xai-grok-shell --locked --bin chat-history-downgrade
    ```

    All of the above must pass before continuing.

12. **Range-diff sanity check.** After a clean rebase, the only difference between the old
    and new gx commit series should be where the patches now apply — not what they do.

    ```
    git range-diff "$upstream_old..$gx_old" upstream/main..gx/main
    ```

    Expect only patch-movement noise (context line shifts). Any content-level diff here
    (a hunk that materially changed) is a signal to go re-read that commit before pushing.

13. **Push with `--force-with-lease`, never a bare force.** `gx/main` is rewritten by every
    sync — `--force-with-lease` fails safely if someone else pushed to `gx/main` since you
    fetched, instead of silently clobbering their work.

    ```
    git push --force-with-lease origin gx/main
    ```

    After this, any local branch based on the pre-sync `gx/main` must be rebased with
    `git rebase --onto gx/main <old_gx_main_sha> <branch>` — a plain `git pull` on top of
    a force-pushed branch will not do the right thing.

## `scripts/gx/sync-upstream.sh`

Runs steps 1–13 mechanically, echoing each step as it goes, aborting loudly (non-zero
exit, clear message) on any unexpected condition rather than guessing. Flags:

- `--dry-run` — run through step 7 (fast-forwarding `main` to `upstream/main`) and stop
  **before** step 8, the actual rebase of `gx/main`. Lets you see whether upstream moved,
  whether history was rewritten (step 4), and get a backup ref made, without touching
  `gx/main` at all.
- `--push` — required to actually push at the end (step 13). Without it, the script does
  everything through the range-diff check and leaves the push to you.

The script never pushes without `--push` explicitly passed, and never uses a bare
`git push --force`.
