# Handoff index — the Manguebyte family (2026-11-23)

From 2026-11-23 these projects are worked on through GitHub, driven by `gh`
from Alesson's PC, with no clone kept on its disk: the CI builds and tests,
and its artifacts carry what Alesson tries by hand. Each repository has
`docs/handoff/HANDOFF.md`: where things stand, what is open, the decisions,
the traps and what still needs the PC.

| Project | Repository | Visibility | Work branch | CI |
|---|---|---|---|---|
| Manguebyte (framework) | `alessonqueirozdev-hub/manguebyte` | public | `main` (protected: 29 checks) | PRs and `ci/**` |
| Caecilia (organ) | `alessonqueirozdev-hub/Caecilia` | public | `rust-2.0` | PRs into `rust-2.0`/`main`; artifact `Caecilia-windows` |
| Aratu (live coding) | `alessonqueirozdev-hub/aratu` | public | `main` | `ci.yml`, `pages.yml` |
| MARÉ (driver protocol) | `alessonqueirozdev-hub/mare` | public | `main` | `ci/**` branches only |
| Parabolic (plugin generator) | `alessonqueirozdev-hub/parabolic` | private | `main` | none yet (limited minutes) |
| manguebyte-site | `alessonqueirozdev-hub/manguebyte-site` | public | `main` (a push deploys) | `pages.yml` |
| Manager's desk | `alessonqueirozdev-hub/manguebyte-handoff` | private | `main` | — |

## How the projects fit

- Caecilia and Parabolic build against Manguebyte's source by path.
- MARÉ takes Manguebyte from git at a fixed revision.
- Devin implements on Alesson's PC; the manager session audits. Devin's
  briefs, results, audits and logs are in `manguebyte-handoff`.

## What only the PC can do

Listening and playing, real audio and MIDI devices, DAWs, and Devin.
