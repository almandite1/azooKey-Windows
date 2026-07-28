# Security Policy

## Why this matters here

azooKey for Windows is a Text Services Framework input method. Its TIP
(`azookey.dll` / `azookey32.dll`) is an in-process COM server that Windows
loads into **every application where you type** — browsers, editors, password
managers, terminals. It sees keystrokes before the application does, and a
defect in it runs inside the host process.

The rest of the stack is deliberately kept out of that position: conversion
runs in a separate `azookey-server.exe`, and the candidate window in `ui.exe`,
so a crash or hang there cannot take the host application down with it. Reports
about any of these are welcome, but the TIP is where impact is highest.

## Supported versions

Only the most recent release is supported. This is pre-1.0 software under
active development; fixes land on `dev` and reach users in the next release
rather than as backports.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting:
<https://github.com/almandite1/azooKey-Windows/security/advisories/new>

If that is unavailable to you, email almandite@protonmail.com instead.

Useful things to include, as far as you can establish them:

- Which component is affected (TIP DLL, server, UI, launcher, installer)
- Windows version, and the host application if the problem is
  application-specific
- The release or commit you tested
- Steps to reproduce, and what an attacker gains

You can expect an acknowledgement within a week. Because this project is
maintained by one person in their own time, a fix may take longer than that;
you will be told where things stand rather than left waiting.

## Disclosure

Report privately, and please give the fix a chance to ship before publishing.
If you would like credit in the release notes, say so — it will be given unless
you ask otherwise.

## What is in a release

Every release carries a software bill of materials beside the installer:

- `azookey-<version>.cdx.json` — CycloneDX. The complete list: the Rust and npm
  dependencies, the Swift packages the conversion engine is built from, the two
  dictionary submodules, and the binaries that ship without a manifest of their
  own (the llama.cpp backends, the zenz model, the Swift runtime) with the
  SHA256 of each download.
- `azookey-<version>.spdx.json` — SPDX, for tooling that wants that format.
  Rust and npm only; the CycloneDX file is the complete one, and says so in its
  own metadata.

Both feed a vulnerability scanner (`grype`, `osv-scanner`) directly. The
WebView2 runtime and the Visual C++ redistributable are *not* listed: the
installer fetches them from Microsoft at install time, so they are not part of
what is distributed here.

`gh api repos/almandite1/azooKey-Windows/dependency-graph/sbom` is a second,
independent source, produced by GitHub from the lockfiles. It covers the Rust
and npm halves only.

## Scope notes

Some things that look like vulnerabilities in an IME are known properties of
this one:

- **The TIP is not code-signed yet.** Security software may block it from
  loading into protected processes. This is tracked in
  [issue #13](https://github.com/almandite1/azooKey-Windows/issues/13).
- **Conversion is entirely local.** No keystroke, reading, or candidate leaves
  the machine. There is no telemetry, and nothing contacts the network on its
  own — the settings app's update entry is a link you click, which opens the
  releases page in your browser.
- **Logs.** Debug builds write per-application logs under
  `%LOCALAPPDATA%\Azookey\logs`, which contain what was typed. Release builds
  do not write them. If you find a release build writing keystrokes anywhere,
  that is a bug worth reporting.
