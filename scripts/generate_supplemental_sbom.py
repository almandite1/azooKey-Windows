#!/usr/bin/env python3
"""Complete the CycloneDX SBOM that syft produces for this repository.

syft catalogues what a manifest describes: Cargo.lock and
frontend/package-lock.json, and -- since it learned to read Package.resolved
schema v3 -- the Swift pins as well. Three things it still cannot see end up in
azookey-setup.exe anyway:

  1. Swift pins that carry no version. A branch or revision pin has only a
     commit in Package.resolved, so syft emits the package with version
     "UNKNOWN" and a purl with no version at all. Those entries are completed
     here from the same file. Licenses are added for every Swift pin: they live
     in the upstream repositories, not in Package.resolved, so no cataloguer
     can know them.
  2. The two dictionary submodules. They are shipped verbatim (build/Dictionary
     and build/EmojiDictionary) and have no manifest of any kind.
  3. The prebuilt binaries the workflow downloads: the three llama.cpp backends
     and the zenz model, plus the Swift runtime DLLs the installer bundles.
     These have no manifest either, so they are described by hand in
     scripts/sbom_components.json.

For (3) the checksums exist twice -- once in the workflow that verifies the
download, once in the components file -- which is exactly the kind of
duplication that rots. So it is checked rather than trusted: every SHA256 in
the components file must equal the one the workflow pins, and every SHA256 the
workflow pins must be claimed by a component (or be listed as deliberately
unclaimed). Either half moving alone fails the build.

Run with no --merge to see just the supplemental components; the CI step passes
syft's document in and writes the merged result back over it.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit

# Package.resolved layouts this script understands. v1 nests the pins under
# "object" and stores a different set of fields; if it ever comes back the
# parser below must be told about it rather than silently reading nothing.
SUPPORTED_PIN_SCHEMA_VERSIONS = (2, 3)

# What CycloneDX calls the SHA-256 algorithm. Spelled out because "SHA256" is
# not a valid value and the schema will reject it.
HASH_ALG_SHA256 = "SHA-256"

DEFAULT_SPEC_VERSION = "1.6"
CDX_SCHEMA_URL = "http://cyclonedx.org/schema/bom-{version}.schema.json"

PRODUCT_NAME = "azooKey for Windows"
PRODUCT_BOM_REF = "azookey-windows-setup"


class SbomError(Exception):
    """A problem the caller has to fix -- reported without a traceback."""


# --------------------------------------------------------------------------
# small helpers
# --------------------------------------------------------------------------


def license_entry(value):
    """One CycloneDX `licenses` entry from a plain string.

    A bare SPDX identifier goes in as `id`, which the schema validates against
    the SPDX list -- a typo there fails validation rather than shipping a
    license nobody can look up. Anything with an SPDX operator in it is an
    expression, and anything unknown is NOASSERTION, which is a claim about
    our knowledge rather than about the license.
    """
    if not value or value == "NOASSERTION":
        return {"license": {"name": "NOASSERTION"}}
    if re.search(r"\s(?:WITH|AND|OR)\s", value):
        return {"expression": value}
    return {"license": {"id": value}}


def read_workspace_version(cargo_toml_text: str) -> str:
    """The single source of the product version -- [workspace.package] in the
    root Cargo.toml, the same value build_installer reads."""
    match = re.search(
        r"^\s*\[workspace\.package\]\s*$(.*?)(?=^\s*\[|\Z)",
        cargo_toml_text,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise SbomError("no [workspace.package] section in Cargo.toml")
    version = re.search(r'^\s*version\s*=\s*"([^"]+)"', match.group(1), re.MULTILINE)
    if version is None:
        raise SbomError("no version key under [workspace.package] in Cargo.toml")
    return version.group(1)


# --------------------------------------------------------------------------
# Swift pins (server-swift/Package.resolved)
# --------------------------------------------------------------------------


def parse_package_resolved(text: str) -> list[dict]:
    """Normalise Package.resolved into {identity, location, version, revision}.

    `version` is None for a branch or revision pin; every caller has to decide
    what to do about that rather than inventing a version here.
    """
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise SbomError(f"Package.resolved is not valid JSON: {error}") from error

    schema_version = document.get("version")
    if schema_version not in SUPPORTED_PIN_SCHEMA_VERSIONS:
        raise SbomError(
            f"Package.resolved schema version {schema_version!r} is not supported "
            f"(known: {', '.join(str(v) for v in SUPPORTED_PIN_SCHEMA_VERSIONS)}). "
            "Teach parse_package_resolved() the new layout before the Swift "
            "dependencies quietly drop out of the SBOM."
        )

    pins = []
    for pin in document.get("pins", []):
        state = pin.get("state", {})
        identity = pin.get("identity")
        location = pin.get("location")
        if not identity or not location:
            raise SbomError(f"pin without identity or location: {pin!r}")
        pins.append(
            {
                "identity": identity,
                "location": location,
                "version": state.get("version"),
                "revision": state.get("revision"),
                "branch": state.get("branch"),
            }
        )
    if not pins:
        raise SbomError("Package.resolved lists no pins")
    return pins


def swift_purl(location: str, version: str) -> str:
    """`pkg:swift/<host>/<org>/<name>@<version>`.

    The swift purl type identifies a package by where it is fetched from, so
    the namespace is the host plus the owning organisation. Case is kept as the
    URL spells it, which is what the purl specification's own Swift example
    does; syft lowercases the whole thing, which is why the enrichment below
    matches case-insensitively.
    """
    parts = urlsplit(location if "://" in location else f"https://{location}")
    host = parts.netloc.lower()
    path = parts.path.strip("/")
    if path.endswith(".git"):
        path = path[: -len(".git")]
    segments = [segment for segment in path.split("/") if segment]
    if not host or len(segments) < 2:
        raise SbomError(f"cannot build a swift purl from location {location!r}")
    namespace = "/".join([host] + segments[:-1])
    return f"pkg:swift/{namespace}/{segments[-1]}@{version}"


def swift_component(pin: dict, license_value) -> dict:
    """A component for a Swift pin syft did not produce one for."""
    version = pin["version"] or pin["revision"]
    if not version:
        raise SbomError(
            f"Swift pin {pin['identity']!r} has neither a version nor a revision"
        )
    name = pin["location"].rstrip("/").rsplit("/", 1)[-1]
    if name.endswith(".git"):
        name = name[: -len(".git")]
    component = {
        "bom-ref": swift_purl(pin["location"], version),
        "type": "library",
        "name": name,
        "version": version,
        "purl": swift_purl(pin["location"], version),
        "licenses": [license_entry(license_value)],
        "externalReferences": [{"type": "vcs", "url": pin["location"]}],
    }
    component["properties"] = swift_properties(pin)
    return component


def swift_properties(pin: dict) -> list[dict]:
    properties = [{"name": "azookey:sbom:source", "value": "server-swift/Package.resolved"}]
    if pin.get("revision"):
        properties.append({"name": "azookey:swift:revision", "value": pin["revision"]})
    if pin.get("branch"):
        properties.append({"name": "azookey:swift:branch", "value": pin["branch"]})
    return properties


def apply_swift_pins(components: list[dict], pins: list[dict], licenses: dict) -> dict:
    """Complete syft's Swift entries in place, adding any it missed.

    syft keys these off the pin identity and lowercases it into the purl, so a
    case-insensitive name match is the reliable join. A pin without a version
    reaches this point as version "UNKNOWN" and a purl with no version: both
    are rewritten from the revision, because an entry nothing can be looked up
    by is worse than no entry.
    """
    by_name = {}
    for component in components:
        purl = component.get("purl", "")
        if purl.startswith("pkg:swift/") or component.get("type") == "library":
            by_name.setdefault(str(component.get("name", "")).lower(), []).append(component)

    added, enriched = [], []
    for pin in pins:
        license_value = licenses.get(pin["identity"], None)
        candidates = [
            component
            for component in by_name.get(pin["identity"].lower(), [])
            if str(component.get("purl", "")).startswith("pkg:swift/")
        ]
        if not candidates:
            components.append(swift_component(pin, license_value))
            added.append(pin["identity"])
            continue

        version = pin["version"] or pin["revision"]
        for component in candidates:
            if not component.get("version") or component["version"] == "UNKNOWN":
                component["version"] = version
            if "@" not in str(component.get("purl", "")):
                component["purl"] = f"{component['purl']}@{version}"
            component.setdefault("licenses", [license_entry(license_value)])
            references = component.setdefault("externalReferences", [])
            if not any(reference.get("type") == "vcs" for reference in references):
                references.append({"type": "vcs", "url": pin["location"]})
            component.setdefault("properties", []).extend(swift_properties(pin))
            enriched.append(pin["identity"])
    return {"added": added, "enriched": enriched}


# --------------------------------------------------------------------------
# submodules (.gitmodules + the commit the superproject records)
# --------------------------------------------------------------------------


def parse_gitmodules(text: str) -> list[dict]:
    """{name, path, url} per submodule, from the .gitmodules INI."""
    modules, current = [], None
    for line in text.splitlines():
        line = line.strip()
        header = re.match(r'^\[submodule\s+"(.+)"\]$', line)
        if header:
            current = {"name": header.group(1), "path": None, "url": None}
            modules.append(current)
            continue
        entry = re.match(r"^(\w+)\s*=\s*(.+)$", line)
        if entry and current is not None and entry.group(1) in ("path", "url"):
            current[entry.group(1)] = entry.group(2).strip()
    for module in modules:
        if not module["path"] or not module["url"]:
            raise SbomError(f"submodule {module['name']!r} has no path or no url")
    return modules


def parse_ls_tree(text: str) -> dict:
    """path -> commit, from `git ls-tree -r HEAD` gitlink entries.

    The commit recorded in the superproject is the one that gets built, which
    is not necessarily the one a working tree happens to have checked out.
    """
    commits = {}
    for line in text.splitlines():
        match = re.match(r"^\d+\s+commit\s+([0-9a-f]{40})\s+(.+)$", line.strip())
        if match:
            commits[match.group(2).strip()] = match.group(1)
    return commits


def submodule_commits(repo_root: Path) -> dict:
    result = subprocess.run(
        ["git", "-C", str(repo_root), "ls-tree", "-r", "HEAD"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SbomError(f"git ls-tree failed: {result.stderr.strip()}")
    return parse_ls_tree(result.stdout)


def github_purl(url: str, version: str) -> str:
    """`pkg:github/<owner>/<repo>@<commit>` -- lowercased, as that purl type
    requires."""
    parts = urlsplit(url if "://" in url else f"https://{url}")
    path = parts.path.strip("/")
    if path.endswith(".git"):
        path = path[: -len(".git")]
    segments = [segment for segment in path.split("/") if segment]
    if len(segments) != 2:
        raise SbomError(f"cannot build a github purl from {url!r}")
    return f"pkg:github/{segments[0].lower()}/{segments[1].lower()}@{version}"


def submodule_component(module: dict, commit: str, spec: dict) -> dict:
    metadata = spec.get(module["path"], {})
    name = module["path"].rsplit("/", 1)[-1]
    component = {
        "bom-ref": github_purl(module["url"], commit),
        "type": "data" if metadata.get("kind") == "data" else "library",
        "name": name,
        "version": commit,
        "purl": github_purl(module["url"], commit),
        "licenses": [license_entry(metadata.get("license"))],
        "externalReferences": [{"type": "vcs", "url": module["url"]}],
        "properties": [
            {"name": "azookey:sbom:source", "value": ".gitmodules"},
            {"name": "azookey:submodule:path", "value": module["path"]},
        ],
    }
    if metadata.get("description"):
        component["description"] = metadata["description"]
    if metadata.get("bundled-as"):
        component["properties"].append(
            {"name": "azookey:bundled-as", "value": metadata["bundled-as"]}
        )
    return component


# --------------------------------------------------------------------------
# hand-described bundled binaries, cross-checked against the workflow
# --------------------------------------------------------------------------


def parse_workflow_sha256_env(text: str) -> dict:
    """The `<NAME>_SHA256: <hex>` pins from the workflow's env block.

    Read with a regular expression rather than a YAML parser on purpose: this
    has to run on a bare Python with nothing installed, and the shape it is
    looking for is fixed by the workflow right next to it.
    """
    return {
        match.group(1): match.group(2).upper()
        for match in re.finditer(
            r"^\s*([A-Z0-9_]+_SHA256)\s*:\s*['\"]?([0-9A-Fa-f]{64})['\"]?\s*$",
            text,
            re.MULTILINE,
        )
    }


def verify_against_workflow(spec: dict, workflow_text: str) -> None:
    """Fail unless the components file and the workflow say the same thing.

    Both directions are checked. A component whose checksum drifted from the
    workflow's is the obvious case; a checksum in the workflow that no
    component claims is the one that actually bites, because that is a new
    download shipping with nothing in the SBOM to describe it.
    """
    workflow_hashes = parse_workflow_sha256_env(workflow_text)
    claimed = set(spec.get("unclaimed_workflow_sha256_env", []))

    for component in spec.get("bundled", []):
        verify = component.get("verify", {})
        key = verify.get("sha256_from_workflow_env")
        if key:
            claimed.add(key)
            if key not in workflow_hashes:
                raise SbomError(
                    f"{component['name']}: the components file expects "
                    f"{key} in the workflow env block, which does not define it"
                )
            expected = workflow_hashes[key]
            actual = str(component.get("sha256", "")).upper()
            if actual != expected:
                raise SbomError(
                    f"{component['name']}: SHA256 mismatch. "
                    f"scripts/sbom_components.json says {actual or '(none)'}, "
                    f"the workflow's {key} says {expected}. "
                    "One of the two was updated without the other."
                )
        pattern = verify.get("version_from_workflow_regex")
        if pattern:
            match = re.search(pattern, workflow_text)
            if match is None:
                raise SbomError(
                    f"{component['name']}: no match for {pattern!r} in the workflow"
                )
            if match.group(1) != component.get("version"):
                raise SbomError(
                    f"{component['name']}: version mismatch. "
                    f"scripts/sbom_components.json says {component.get('version')!r}, "
                    f"the workflow says {match.group(1)!r}."
                )

    unclaimed = sorted(set(workflow_hashes) - claimed)
    if unclaimed:
        raise SbomError(
            "the workflow pins checksums nothing in the SBOM accounts for: "
            + ", ".join(unclaimed)
            + ". Add a component to scripts/sbom_components.json, or list the "
            "name under unclaimed_workflow_sha256_env if it is a build tool "
            "that does not ship."
        )


def bundled_component(entry: dict) -> dict:
    purl = entry.get("purl", "")
    if not purl.startswith("pkg:"):
        raise SbomError(f"{entry.get('name')!r}: purl must start with 'pkg:'")
    component = {
        "bom-ref": purl,
        "type": entry.get("type", "library"),
        "name": entry["name"],
        "version": entry["version"],
        "purl": purl,
        "licenses": [license_entry(entry.get("license"))],
    }
    if entry.get("description"):
        component["description"] = entry["description"]
    if entry.get("supplier"):
        component["supplier"] = {"name": entry["supplier"]}
    if entry.get("sha256"):
        component["hashes"] = [
            {"alg": HASH_ALG_SHA256, "content": entry["sha256"].lower()}
        ]
    references = []
    if entry.get("url"):
        references.append({"type": "distribution", "url": entry["url"]})
    if entry.get("vcs"):
        references.append({"type": "vcs", "url": entry["vcs"]})
    if references:
        component["externalReferences"] = references
    properties = [{"name": "azookey:sbom:source", "value": "scripts/sbom_components.json"}]
    if entry.get("bundled-as"):
        properties.append({"name": "azookey:bundled-as", "value": entry["bundled-as"]})
    component["properties"] = properties
    return component


# --------------------------------------------------------------------------
# document assembly
# --------------------------------------------------------------------------


def empty_document(spec_version: str) -> dict:
    return {
        "$schema": CDX_SCHEMA_URL.format(version=spec_version),
        "bomFormat": "CycloneDX",
        "specVersion": spec_version,
        "version": 1,
        "metadata": {},
        "components": [],
    }


def set_product_metadata(document: dict, version: str, timestamp: str, notes: list) -> None:
    """Name the thing the SBOM is about: the installer, not the directory syft
    happened to scan."""
    metadata = document.setdefault("metadata", {})
    metadata["timestamp"] = timestamp

    previous_ref = (metadata.get("component") or {}).get("bom-ref")
    metadata["component"] = {
        "bom-ref": PRODUCT_BOM_REF,
        "type": "application",
        "name": PRODUCT_NAME,
        "version": version,
        "description": "Japanese input method for Windows (TSF text service)",
        "licenses": [license_entry("MIT")],
        "externalReferences": [
            {"type": "vcs", "url": "https://github.com/almandite1/azooKey-Windows"},
            {
                "type": "distribution",
                "url": "https://github.com/almandite1/azooKey-Windows/releases",
            },
        ],
    }
    # syft names the scanned directory as the root component and the dependency
    # graph refers to it by that id; leaving a dangling ref behind would fail
    # validation.
    if previous_ref and previous_ref != PRODUCT_BOM_REF:
        for dependency in document.get("dependencies", []):
            if dependency.get("ref") == previous_ref:
                dependency["ref"] = PRODUCT_BOM_REF
            depends_on = dependency.get("dependsOn")
            if depends_on:
                dependency["dependsOn"] = [
                    PRODUCT_BOM_REF if ref == previous_ref else ref for ref in depends_on
                ]

    tools = metadata.setdefault("tools", {})
    if isinstance(tools, dict):
        tools.setdefault("components", []).append(
            {
                "type": "application",
                "name": "generate_supplemental_sbom.py",
                "version": version,
            }
        )

    metadata["properties"] = notes


def scope_notes() -> list:
    """What this SBOM covers, said in the document itself rather than only in a
    plan file nobody downloads with it."""
    return [
        {
            "name": "azookey:sbom:scope",
            "value": (
                "The contents of azookey-setup.exe: the TIP DLLs, the server and "
                "its Swift engine, the candidate window, the launcher, the "
                "settings app, the dictionaries, the zenz model and the llama.cpp "
                "backends."
            ),
        },
        {
            "name": "azookey:sbom:excluded",
            "value": (
                "The WebView2 runtime and the Visual C++ redistributable are "
                "fetched by the installer at install time "
                "(installer/CodeDependencies.iss) and are not part of the "
                "distributed artefact."
            ),
        },
        {
            "name": "azookey:sbom:known-limitation",
            "value": (
                "Cargo components come from Cargo.lock and are a superset of what "
                "ships: dev- and build-only dependencies are included. npm "
                "components exclude dev dependencies."
            ),
        },
    ]


def generate(
    repo_root: Path,
    spec: dict,
    base_document: dict,
    product_version: str,
    timestamp: str,
) -> tuple:
    package_resolved = (repo_root / "server-swift" / "Package.resolved").read_text(
        encoding="utf-8"
    )
    workflow = (repo_root / ".github" / "workflows" / "actions.yml").read_text(
        encoding="utf-8"
    )
    gitmodules = (repo_root / ".gitmodules").read_text(encoding="utf-8")

    verify_against_workflow(spec, workflow)

    document = base_document
    components = document.setdefault("components", [])

    swift_result = apply_swift_pins(
        components, parse_package_resolved(package_resolved), spec.get("swift_licenses", {})
    )

    commits = submodule_commits(repo_root)
    submodules = parse_gitmodules(gitmodules)
    for module in submodules:
        commit = commits.get(module["path"])
        if commit is None:
            raise SbomError(
                f"{module['path']} is in .gitmodules but HEAD records no commit "
                "for it"
            )
        components.append(submodule_component(module, commit, spec.get("submodules", {})))

    for entry in spec.get("bundled", []):
        components.append(bundled_component(entry))

    set_product_metadata(document, product_version, timestamp, scope_notes())

    summary = {
        "swift_added": swift_result["added"],
        "swift_enriched": swift_result["enriched"],
        "submodules": [module["path"] for module in submodules],
        "bundled": [entry["name"] for entry in spec.get("bundled", [])],
        "total_components": len(components),
    }
    return document, summary


def write_atomically(path: Path, text: str) -> None:
    """The CI step merges syft's document back over itself; a half-written file
    there would be an SBOM that validates as nothing at all."""
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temporary = tempfile.mkstemp(dir=str(path.parent), suffix=".tmp")
    try:
        with os.fdopen(handle, "w", encoding="utf-8", newline="\n") as stream:
            stream.write(text)
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description="Add the components syft cannot see to a CycloneDX SBOM."
    )
    default_root = Path(__file__).resolve().parent.parent
    parser.add_argument(
        "--repo-root", type=Path, default=default_root, help="repository root"
    )
    parser.add_argument(
        "--components",
        type=Path,
        default=None,
        help="the hand-maintained component table (default: scripts/sbom_components.json)",
    )
    parser.add_argument(
        "--merge",
        type=Path,
        default=None,
        help="a CycloneDX JSON document from syft to merge into",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="where to write the result (default: stdout). May be the --merge file.",
    )
    parser.add_argument(
        "--product-version",
        default=None,
        help="version of the product (default: [workspace.package] in Cargo.toml)",
    )
    parser.add_argument(
        "--timestamp",
        default=None,
        help="metadata.timestamp, ISO 8601 (default: now, UTC)",
    )
    arguments = parser.parse_args(argv)

    repo_root = arguments.repo_root.resolve()
    components_path = arguments.components or (repo_root / "scripts" / "sbom_components.json")

    try:
        spec = json.loads(components_path.read_text(encoding="utf-8"))
        version = arguments.product_version or read_workspace_version(
            (repo_root / "Cargo.toml").read_text(encoding="utf-8")
        )
        timestamp = arguments.timestamp or datetime.now(timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )

        if arguments.merge:
            base = json.loads(arguments.merge.read_text(encoding="utf-8"))
            if base.get("bomFormat") != "CycloneDX":
                raise SbomError(f"{arguments.merge} is not a CycloneDX document")
        else:
            base = empty_document(DEFAULT_SPEC_VERSION)

        document, summary = generate(repo_root, spec, base, version, timestamp)
    except SbomError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    text = json.dumps(document, indent=2, ensure_ascii=False) + "\n"
    if arguments.output:
        write_atomically(arguments.output, text)
    else:
        sys.stdout.write(text)

    print(
        "supplemental SBOM: "
        f"{len(summary['swift_enriched'])} Swift pins completed, "
        f"{len(summary['swift_added'])} added, "
        f"{len(summary['submodules'])} submodules, "
        f"{len(summary['bundled'])} bundled binaries; "
        f"{summary['total_components']} components total",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
