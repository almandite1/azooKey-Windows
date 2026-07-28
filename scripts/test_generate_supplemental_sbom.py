#!/usr/bin/env python3
"""Tests for generate_supplemental_sbom.py.

    python -m unittest discover -s scripts

Plain unittest, no pytest: this runs on a CI runner with nothing installed,
and the point of the script is to fail a build, so its own test must not need
a package install to answer.

The last test is the one that earns its keep on a normal day: it runs the real
components file against the real workflow, so bumping a download in one place
and not the other fails here before it fails in CI.
"""

from __future__ import annotations

import importlib.util
import json
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MODULE_PATH = REPO_ROOT / "scripts" / "generate_supplemental_sbom.py"

_spec = importlib.util.spec_from_file_location("sbomgen", MODULE_PATH)
sbomgen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sbomgen)


PACKAGE_RESOLVED_V3 = json.dumps(
    {
        "originHash": "0" * 64,
        "pins": [
            {
                "identity": "azookeykanakanjiconverter",
                "kind": "remoteSourceControl",
                "location": "https://github.com/azookey/AzooKeyKanaKanjiConverter",
                "state": {"revision": "bbef9d2d99a2e9e69ac3f7e2e07b08474de59a81"},
            },
            {
                "identity": "swift-numerics",
                "kind": "remoteSourceControl",
                "location": "https://github.com/apple/swift-numerics.git",
                "state": {"revision": "0c0290ff", "version": "1.1.1"},
            },
        ],
        "version": 3,
    }
)


class ParsePackageResolved(unittest.TestCase):
    def test_v3_pins(self):
        pins = sbomgen.parse_package_resolved(PACKAGE_RESOLVED_V3)
        self.assertEqual(["azookeykanakanjiconverter", "swift-numerics"],
                         [pin["identity"] for pin in pins])
        # a revision-only pin has no version; the caller substitutes the commit
        self.assertIsNone(pins[0]["version"])
        self.assertEqual("bbef9d2d99a2e9e69ac3f7e2e07b08474de59a81", pins[0]["revision"])
        self.assertEqual("1.1.1", pins[1]["version"])

    def test_v2_is_accepted(self):
        document = json.loads(PACKAGE_RESOLVED_V3)
        document["version"] = 2
        self.assertEqual(2, len(sbomgen.parse_package_resolved(json.dumps(document))))

    def test_unknown_schema_version_is_refused(self):
        # a silent empty list here would drop every Swift dependency from the
        # SBOM without anything going red
        document = json.loads(PACKAGE_RESOLVED_V3)
        document["version"] = 4
        with self.assertRaises(sbomgen.SbomError) as caught:
            sbomgen.parse_package_resolved(json.dumps(document))
        self.assertIn("not supported", str(caught.exception))

    def test_invalid_json_is_refused(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.parse_package_resolved("{ not json")

    def test_no_pins_is_refused(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.parse_package_resolved(json.dumps({"version": 3, "pins": []}))


class Purls(unittest.TestCase):
    def test_swift_purl_keeps_case_and_strips_dot_git(self):
        self.assertEqual(
            "pkg:swift/github.com/apple/swift-numerics@1.1.1",
            sbomgen.swift_purl("https://github.com/apple/swift-numerics.git", "1.1.1"),
        )
        self.assertEqual(
            "pkg:swift/github.com/azookey/AzooKeyKanaKanjiConverter@abc123",
            sbomgen.swift_purl(
                "https://github.com/azookey/AzooKeyKanaKanjiConverter", "abc123"
            ),
        )

    def test_swift_purl_rejects_a_location_without_an_owner(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.swift_purl("https://github.com/", "1.0.0")

    def test_github_purl_is_lowercased(self):
        self.assertEqual(
            "pkg:github/ensan-hcl/azookey_dictionary_storage@" + "a" * 40,
            sbomgen.github_purl(
                "https://github.com/ensan-hcl/azooKey_dictionary_storage.git", "a" * 40
            ),
        )


class SwiftComponents(unittest.TestCase):
    def test_branch_pin_uses_the_revision_as_its_version(self):
        pin = sbomgen.parse_package_resolved(PACKAGE_RESOLVED_V3)[0]
        component = sbomgen.swift_component(pin, "MIT")
        self.assertEqual("bbef9d2d99a2e9e69ac3f7e2e07b08474de59a81", component["version"])
        self.assertTrue(component["purl"].endswith("@bbef9d2d99a2e9e69ac3f7e2e07b08474de59a81"))
        self.assertEqual([{"license": {"id": "MIT"}}], component["licenses"])

    def test_pin_without_version_or_revision_is_refused(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.swift_component(
                {
                    "identity": "x",
                    "location": "https://github.com/o/x",
                    "version": None,
                    "revision": None,
                },
                "MIT",
            )

    def test_syft_entry_is_completed_rather_than_duplicated(self):
        # what syft emits for a revision-only pin: no version in the purl and
        # the literal string UNKNOWN in the version field
        components = [
            {
                "bom-ref": "1e8c33c0e582982b",
                "type": "library",
                "name": "azookeykanakanjiconverter",
                "version": "UNKNOWN",
                "purl": "pkg:swift/github.com/azookey/azookeykanakanjiconverter",
            }
        ]
        pins = sbomgen.parse_package_resolved(PACKAGE_RESOLVED_V3)
        result = sbomgen.apply_swift_pins(components, pins, {"azookeykanakanjiconverter": "MIT"})

        self.assertEqual(["azookeykanakanjiconverter"], result["enriched"])
        self.assertEqual(["swift-numerics"], result["added"])
        # one completed in place, one appended -- not three
        self.assertEqual(2, len(components))

        completed = components[0]
        revision = "bbef9d2d99a2e9e69ac3f7e2e07b08474de59a81"
        self.assertEqual(revision, completed["version"])
        self.assertEqual(
            "pkg:swift/github.com/azookey/azookeykanakanjiconverter@" + revision,
            completed["purl"],
        )
        self.assertEqual([{"license": {"id": "MIT"}}], completed["licenses"])
        # the bom-ref is left alone: the dependency graph refers to it
        self.assertEqual("1e8c33c0e582982b", completed["bom-ref"])
        self.assertIn(
            {"name": "azookey:swift:revision", "value": revision},
            completed["properties"],
        )

    def test_an_already_versioned_entry_only_gains_a_license(self):
        components = [
            {
                "bom-ref": "ref",
                "type": "library",
                "name": "swift-numerics",
                "version": "1.1.1",
                "purl": "pkg:swift/github.com/apple/swift-numerics@1.1.1",
            }
        ]
        pins = [pin for pin in sbomgen.parse_package_resolved(PACKAGE_RESOLVED_V3)
                if pin["identity"] == "swift-numerics"]
        sbomgen.apply_swift_pins(components, pins, {"swift-numerics": "Apache-2.0"})
        self.assertEqual(1, len(components))
        self.assertEqual("1.1.1", components[0]["version"])
        self.assertEqual([{"license": {"id": "Apache-2.0"}}], components[0]["licenses"])

    def test_an_unlisted_license_becomes_noassertion(self):
        components = []
        pins = sbomgen.parse_package_resolved(PACKAGE_RESOLVED_V3)
        sbomgen.apply_swift_pins(components, pins, {})
        self.assertEqual(
            [{"license": {"name": "NOASSERTION"}}], components[0]["licenses"]
        )


class Submodules(unittest.TestCase):
    GITMODULES = (
        '[submodule "server-swift/azooKey_dictionary_storage"]\n'
        "\tpath = server-swift/azooKey_dictionary_storage\n"
        "\turl = https://github.com/ensan-hcl/azooKey_dictionary_storage.git\n"
    )

    def test_parse_gitmodules(self):
        modules = sbomgen.parse_gitmodules(self.GITMODULES)
        self.assertEqual(1, len(modules))
        self.assertEqual("server-swift/azooKey_dictionary_storage", modules[0]["path"])

    def test_submodule_without_a_path_is_refused(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.parse_gitmodules('[submodule "x"]\n\turl = https://example.com/x\n')

    def test_parse_ls_tree_takes_only_gitlinks(self):
        text = (
            "100644 blob 1111111111111111111111111111111111111111\tCargo.toml\n"
            "160000 commit 2222222222222222222222222222222222222222\tserver-swift/dict\n"
        )
        self.assertEqual(
            {"server-swift/dict": "2" * 40}, sbomgen.parse_ls_tree(text)
        )

    def test_component_uses_the_recorded_commit_as_its_version(self):
        module = sbomgen.parse_gitmodules(self.GITMODULES)[0]
        component = sbomgen.submodule_component(
            module,
            "b" * 40,
            {
                "server-swift/azooKey_dictionary_storage": {
                    "kind": "data",
                    "license": None,
                    "bundled-as": "Dictionary/",
                }
            },
        )
        self.assertEqual("data", component["type"])
        self.assertEqual("b" * 40, component["version"])
        self.assertEqual([{"license": {"name": "NOASSERTION"}}], component["licenses"])


class WorkflowCrossCheck(unittest.TestCase):
    WORKFLOW = (
        "env:\n"
        "  LLAMA_CPU_SHA256: " + "A" * 64 + "\n"
        "  INNOSETUP_SHA256: " + "B" * 64 + "\n"
        "      with:\n"
        "        swift-version: swift-6.3.3-release\n"
    )

    def spec(self, **overrides):
        component = {
            "name": "llama.cpp (AVX CPU backend)",
            "version": "b4846",
            "purl": "pkg:github/fkunn1326/llama.cpp@b4846",
            "sha256": "a" * 64,
            "verify": {"sha256_from_workflow_env": "LLAMA_CPU_SHA256"},
        }
        component.update(overrides)
        return {
            "bundled": [component],
            "unclaimed_workflow_sha256_env": ["INNOSETUP_SHA256"],
        }

    def test_parse_workflow_sha256_env(self):
        self.assertEqual(
            {"LLAMA_CPU_SHA256": "A" * 64, "INNOSETUP_SHA256": "B" * 64},
            sbomgen.parse_workflow_sha256_env(self.WORKFLOW),
        )

    def test_matching_checksums_pass(self):
        sbomgen.verify_against_workflow(self.spec(), self.WORKFLOW)

    def test_a_drifted_checksum_fails(self):
        with self.assertRaises(sbomgen.SbomError) as caught:
            sbomgen.verify_against_workflow(self.spec(sha256="c" * 64), self.WORKFLOW)
        self.assertIn("SHA256 mismatch", str(caught.exception))

    def test_a_missing_workflow_key_fails(self):
        spec = self.spec(verify={"sha256_from_workflow_env": "LLAMA_VULKAN_SHA256"})
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.verify_against_workflow(spec, self.WORKFLOW)

    def test_a_workflow_checksum_no_component_claims_fails(self):
        # the case that matters: a new download added to CI, nothing in the SBOM
        spec = self.spec()
        spec["unclaimed_workflow_sha256_env"] = []
        with self.assertRaises(sbomgen.SbomError) as caught:
            sbomgen.verify_against_workflow(spec, self.WORKFLOW)
        self.assertIn("INNOSETUP_SHA256", str(caught.exception))

    def test_version_regex_check(self):
        pattern = r"swift-version:\s*swift-([0-9][^-\s]*)-release"
        spec = {
            "bundled": [
                {
                    "name": "Swift runtime for Windows",
                    "version": "6.3.3",
                    "purl": "pkg:github/swiftlang/swift@swift-6.3.3-RELEASE",
                    "verify": {"version_from_workflow_regex": pattern},
                }
            ],
            "unclaimed_workflow_sha256_env": ["LLAMA_CPU_SHA256", "INNOSETUP_SHA256"],
        }
        sbomgen.verify_against_workflow(spec, self.WORKFLOW)

        spec["bundled"][0]["version"] = "6.2.0"
        with self.assertRaises(sbomgen.SbomError) as caught:
            sbomgen.verify_against_workflow(spec, self.WORKFLOW)
        self.assertIn("version mismatch", str(caught.exception))


class BundledComponents(unittest.TestCase):
    def test_checksum_becomes_a_lowercase_cyclonedx_hash(self):
        component = sbomgen.bundled_component(
            {
                "name": "x",
                "version": "1",
                "purl": "pkg:generic/x@1",
                "license": "MIT",
                "sha256": "A" * 64,
            }
        )
        self.assertEqual(
            [{"alg": "SHA-256", "content": "a" * 64}], component["hashes"]
        )

    def test_a_purl_that_is_not_a_purl_is_refused(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.bundled_component({"name": "x", "version": "1", "purl": "x@1"})


class Licenses(unittest.TestCase):
    def test_spdx_id(self):
        self.assertEqual({"license": {"id": "MIT"}}, sbomgen.license_entry("MIT"))

    def test_expression(self):
        self.assertEqual(
            {"expression": "Apache-2.0 WITH Swift-exception"},
            sbomgen.license_entry("Apache-2.0 WITH Swift-exception"),
        )

    def test_unknown(self):
        self.assertEqual(
            {"license": {"name": "NOASSERTION"}}, sbomgen.license_entry(None)
        )


class ProductMetadata(unittest.TestCase):
    def test_workspace_version_is_read_from_the_right_section(self):
        cargo = (
            "[workspace]\n"
            'members = ["a"]\n'
            "\n"
            "[workspace.package]\n"
            'version = "0.1.1-beta.alm"\n'
            "\n"
            "[workspace.dependencies]\n"
            'serde = { version = "1.0.0" }\n'
        )
        self.assertEqual("0.1.1-beta.alm", sbomgen.read_workspace_version(cargo))

    def test_missing_version_is_refused(self):
        with self.assertRaises(sbomgen.SbomError):
            sbomgen.read_workspace_version("[workspace]\n")

    def test_the_root_component_is_replaced_and_its_references_repointed(self):
        document = {
            "metadata": {"component": {"bom-ref": "syft-root", "type": "file", "name": "."}},
            "dependencies": [
                {"ref": "syft-root", "dependsOn": ["pkg:cargo/x@1"]},
                {"ref": "pkg:cargo/x@1", "dependsOn": ["syft-root"]},
            ],
        }
        sbomgen.set_product_metadata(document, "0.1.2", "2026-01-01T00:00:00Z", [])
        self.assertEqual(
            sbomgen.PRODUCT_BOM_REF, document["metadata"]["component"]["bom-ref"]
        )
        self.assertEqual("0.1.2", document["metadata"]["component"]["version"])
        self.assertEqual(sbomgen.PRODUCT_BOM_REF, document["dependencies"][0]["ref"])
        self.assertEqual(
            [sbomgen.PRODUCT_BOM_REF], document["dependencies"][1]["dependsOn"]
        )


class RealFiles(unittest.TestCase):
    """The components file and the workflow have to agree, always."""

    def test_components_file_agrees_with_the_workflow(self):
        spec = json.loads(
            (REPO_ROOT / "scripts" / "sbom_components.json").read_text(encoding="utf-8")
        )
        workflow = (
            REPO_ROOT / ".github" / "workflows" / "actions.yml"
        ).read_text(encoding="utf-8")
        sbomgen.verify_against_workflow(spec, workflow)

    def test_every_bundled_entry_builds_a_component(self):
        spec = json.loads(
            (REPO_ROOT / "scripts" / "sbom_components.json").read_text(encoding="utf-8")
        )
        purls = [sbomgen.bundled_component(entry)["purl"] for entry in spec["bundled"]]
        self.assertEqual(len(purls), len(set(purls)), "bom-refs must be unique")

    def test_the_swift_pins_in_the_repository_all_have_a_license(self):
        spec = json.loads(
            (REPO_ROOT / "scripts" / "sbom_components.json").read_text(encoding="utf-8")
        )
        pins = sbomgen.parse_package_resolved(
            (REPO_ROOT / "server-swift" / "Package.resolved").read_text(encoding="utf-8")
        )
        missing = [
            pin["identity"]
            for pin in pins
            if pin["identity"] not in spec["swift_licenses"]
        ]
        self.assertEqual(
            [],
            missing,
            "add these to swift_licenses in scripts/sbom_components.json, or they "
            "ship as NOASSERTION",
        )


if __name__ == "__main__":
    unittest.main()
