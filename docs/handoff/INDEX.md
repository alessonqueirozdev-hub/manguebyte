# Handoff index — the Manguebyte family (2026-11-23)

Until 2026-11-23 these projects were run from Alesson's Windows PC. Each one
now carries a kit to resume it on Claude Code in the cloud: open a session on
the repository (and branch) below, paste `docs/handoff/PROMPT-RETOMADA.md`.

| Project | Repository | Visibility | Kit on branch | Work branch |
|---|---|---|---|---|
| Manguebyte (framework) | `alessonqueirozdev-hub/manguebyte` | public | `docs/cloud-kit` (PR into `main`) | `main` |
| Caecilia (organ) | `alessonqueirozdev-hub/Caecilia` | public | `docs/cloud-kit` (PR into `rust-2.0`) | `rust-2.0` |
| Aratu (live coding) | `alessonqueirozdev-hub/aratu` | public | `docs/cloud-kit` (PR into `main`) | `main` |
| MARÉ (driver protocol) | `alessonqueirozdev-hub/mare` | public | `docs/cloud-kit` (PR into `main`) | `main` |
| Parabolic (plugin generator) | `alessonqueirozdev-hub/parabolic` | private | `docs/cloud-kit` (PR into `main`) | `main` |
| manguebyte-site | `alessonqueirozdev-hub/manguebyte-site` | public | `docs/cloud-kit` (PR into `main`) | `main` (a push deploys) |
| Manager's handoff folder | `alessonqueirozdev-hub/manguebyte-handoff` | private | `main` | — |

Until each kit's PR is merged, a cloud session checks out `docs/cloud-kit`
first (or reads the files from that branch).

## How the projects fit

- Caecilia and Parabolic build against Manguebyte's source; their setup
  scripts clone it where their manifests expect it.
- MARÉ takes Manguebyte from git at a fixed revision.
- Devin (on Alesson's PC) implements; the manager session audits. Devin's
  briefs, audits and logs are in `manguebyte-handoff`.

## What only the Windows PC can do

Listening and playing, real audio and MIDI devices, DAWs, Windows installers,
and Devin.
