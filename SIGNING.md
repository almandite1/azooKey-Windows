# Code Signing Policy

This document describes who may sign releases of azooKey for Windows, what
gets signed, and where the signed bits come from. It exists because the TIP
(`azookey.dll` / `azookey32.dll`) is loaded into every application where the
user types, and security software reasonably refuses to load an unsigned DLL
into a protected process — see
[issue #13](https://github.com/almandite1/azooKey-Windows/issues/13).

Releases are **not signed yet**. This policy is written ahead of that so the
process is settled before the first signed release, not improvised around it.

## Roles

This fork is maintained by one person, [almandite1](https://github.com/almandite1),
who is therefore the Author, Reviewer and Approver for every signing request.
There is no separation of duties to describe honestly, so none is claimed. If
the project gains maintainers, this section changes before they are given
signing access.

Contact: almandite@protonmail.com

## What is signed

Everything the installer places that Windows executes or loads:

| Artifact | Built from |
|---|---|
| `azookey-setup.exe` | `installer/Installer.iss` (Inno Setup) |
| `azookey.dll`, `azookey32.dll` | `crates/client` — the TSF text input processor, x64 and x86 |
| `azookey-server.exe` | `crates/server` |
| `azookey-server.dll` | `server-swift/` — the Swift conversion engine |
| `ui.exe` | `crates/ui` — candidate window and mode indicator |
| `launcher.exe` | `crates/launcher` — supervisor for server and UI |
| `plugin-host.exe` | `crates/plugin-host` — the add-on host |
| `Azookey.exe` | `frontend/` — the Tauri settings app. The name comes from `mainBinaryName` in `frontend/src-tauri/tauri.conf.json`; without it Tauri names the binary after the crate, and this list stops matching what ships (#97) |

## Provenance

Signed artifacts are built by GitHub Actions from a tag on this repository
(`.github/workflows/actions.yml`), never from a maintainer's machine. Release
assets are the CI build's output; nothing is uploaded by hand.

Two properties of that build are load-bearing for signing, and both predate
this policy:

- **Every `uses:` is pinned to a full commit SHA**, not a tag, because tags can
  be moved after the fact. Dependabot bumps the SHAs.
- **Every downloaded binary dependency is verified against a pinned SHA256** —
  the llama.cpp backends and the conversion model. A moved release or a
  tampered file fails the build instead of shipping quietly.

Reproducing a release locally is documented in the README; the build is
orchestrated by `cargo make build --release`.

## Third-party components

The project's own code is MIT (see `LICENSE`). Bundled components:

| Component | Licence |
|---|---|
| [AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter) | MIT |
| [azooKey_dictionary_storage](https://github.com/ensan-hcl/azooKey_dictionary_storage) (submodule) | Apache-2.0 |
| [azooKey_emoji_dictionary_storage](https://github.com/ensan-hcl/azooKey_emoji_dictionary_storage) (submodule) | **no licence file** — see below |
| llama.cpp backends (`llama_cpu`, `llama_cuda`, `llama_vulkan`) | MIT |
| Swift runtime for Windows | Apache-2.0 with Runtime Library Exception |
| `zenz.gguf` — [zenz-v3.2-small-gguf](https://huggingface.co/Miwa-Keita/zenz-v3.2-small-gguf) | Apache-2.0 |
| Microsoft Visual C++ runtime, WebView2 runtime | Microsoft redistributables, installed as dependencies rather than shipped |

One of these is worth stating plainly rather than leaving for someone to
discover:

**`azooKey_emoji_dictionary_storage` carries no licence file.** It is a public
repository from the same author as the main dictionary, but absence of a
licence is absence of a grant. This should be resolved with the upstream author
rather than assumed.

## Requesting and approving a signature

1. A release is cut by pushing a `v*` tag; CI builds it.
2. The maintainer reviews the diff since the previous tag and the CI run that
   produced the artifacts.
3. Signing is requested for that CI build's artifacts only.
4. Signed artifacts are published as a GitHub Release on this repository.

Nothing is signed from a local build, a branch, or a re-uploaded file.

## Relationship to upstream

This is a visible fork of
[fkunn1326/azooKey-Windows](https://github.com/fkunn1326/azooKey-Windows),
maintained as its own line of development with its own releases. Signatures
produced under this policy cover **this fork's** builds and say nothing about
upstream's.
