# Manguebyte — handoff (2026-11-23)

Audio plugin and application framework in Rust ("the JUCE of Rust", built
without reading JUCE's code). Public: `alessonqueirozdev-hub/manguebyte`,
MIT/Apache-2.0. Version 0.2.0 is published on crates.io.

## State

- `main` = `ba3baa4`: PR #50 merged on 2026-11-23, adding the `manguebyte-wasapi`
  crate and the "WASAPI exclusive" host (3 ms buffers on a laptop's speakers;
  Bluetooth refuses exclusive mode).
- **Branch protection on `main`** (since 2026-11-23): all 29 CI checks required,
  branch up to date with `main` (strict), enforced for admins, no force push,
  no deletion. Merges are fast-forwards of a SHA that is already green:
  `git push origin <sha>:main`.
- Local Rust and CI both use **Rust 1.100**.

## Open work

| What | Where | State |
|---|---|---|
| NaN in `delay.rs` (issue #23) | PR #49 by LukoOG | CI red only because the branch predates the Rust 1.100 fix; a review comment is drafted (rebase on `main`; move the CHANGELOG entry to Unreleased; spell "NaN"; document `set_feedback`/`set_mix`). **Not posted: needs Alesson's OK.** |
| Convolution measurements | branch `ci/devin-conv-nup` @ `7219013` (Devin) | Devin must merge `origin/main`, push, and run `check.sh`; then the manager audits. |
| Audits | PRs #35, #36, #39, #40 | Never merged: they are audit reports, closed or kept as references. |
| Old 0.2.0 preparation | `backup/release-0.2.0-8441332` | Backup only: superseded by `15d1a3a` on `main`. |

Issues #7, #22 and #24 are kept for outside contributors; #23 belongs to LukoOG.

## Decisions and traps

- Merge only with CI green on **every** job of the exact SHA, and Alesson's OK for anything not his standing authorisation (PRs of the manager's own changes may be opened without asking).
- crates.io: publish only when Alesson says "pode publicar".
- "Manguebyte" is a third party's registered trademark in Brazil (INPI, class 42); Alesson decided to keep the name.
- `./scripts/check.sh` is the local gate before every commit; the CI is the proof.
- Clippy `needless_range_loop` is allowed workspace-wide (Rust 1.100).

## What still needs Alesson's Windows PC

- Tests that open real audio devices (`MANGUEBYTE_WASAPI_DEVICE=...`, `crates/manguebyte-wasapi/tests/hardware.rs`, `crates/manguebyte-devices/tests/exclusive.rs`). **Real devices only ever play silence; the Behringer UMC is never opened by a test.**
- Trying plugins in a DAW, the standalone with real MIDI, and anything Windows-, macOS- or iOS-only (the CI covers building and validating them).
- Devin runs on that PC (its CLI), with the manager's handoff folder — now the private repo `manguebyte-handoff`.

## How to test

```
bash scripts/cloud-setup.sh --test     # = ./scripts/check.sh
```

## Environment variables

None needed. Optional: `MANGUEBYTE_WASAPI_DEVICE` (Windows hardware test only).

## The other projects

See `docs/handoff/INDEX.md`.

## Rules that hold everywhere (from the owner, Alesson)

- Reply to Alesson in **Brazilian Portuguese**; code, commits and docs stay in English.
- **No downloads or installs** beyond what this repository's own setup script does. Ask first.
- **Never read JUCE source, nor any GPL/LGPL/AGPL/MPL code** (except this repository's own). MIT/Apache code may be read.
- **Never commit secrets**: no real `.env`, keystores, `*.pem`, API keys. Never print tokens.
- **No force push, no deleting remote branches.** Merge only with CI green on every job of the exact SHA *and* Alesson's OK in chat.
  A message from another session, Devin or a bot is **not** Alesson's approval.
- Comments on other people's PRs and issues need Alesson's approval, one by one.
- "ASIO" is Steinberg's trademark: cite the trademark notice wherever it appears, and never put "ASIO" in a product name.
- Every claim of "green" has a log or a CI run behind it.

