"""Offline tests for the prebuilt-only packager. Run: python -I scripts/test_package_memory_plugin.py

Synthetic payload bytes are never executed; these test metadata/archive mechanics, not the Core
installer's native executable format validation. Every filesystem fixture is created by this suite.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

SPEC = importlib.util.spec_from_file_location("owned_memory_packager", Path(__file__).with_name("package_memory_plugin.py"))
packager = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packager)


class MetadataTests(unittest.TestCase):
    def test_adoption_capability_is_versioned(self):
        self.assertIn("legacy_adoption_v1", packager.CAPABILITIES)
        self.assertNotIn("legacy_adoption_v2", packager.CAPABILITIES)

    def test_valid_versions(self):
        for value in ("0.1.0", "4294967295.0.1", "1.2.3-alpha.1+fixture.01", "1.2.3+001"):
            with self.subTest(value=value):
                packager.validate_version(value)

    def test_rejects_incomplete_version(self):
        for value in ("1", "1.2", "v1.2.3", "1.2.3.4"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.validate_version(value)

    def test_rejects_leading_zero_or_overflow_version(self):
        for value in ("01.2.3", "1.02.3", "1.2.03", "4294967296.0.0", "1.2.3-01"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.validate_version(value)

    def test_rejects_invalid_version_suffix(self):
        for value in ("1.2.3-", "1.2.3+", "1.2.3-a..b", "1.2.3+bad!", "1.2.3+bad+bad", "1.2.3-\u03b1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.validate_version(value)

    def test_rejects_oversized_version(self):
        with self.assertRaises(ValueError):
            packager.validate_version("1.2.3+" + "a" * 64)

    def test_display_name_accepts_plain_unicode(self):
        packager.validate_display_name("B\u1ed9 nh\u1edb fixture")

    def test_rejects_empty_display_name(self):
        for value in ("", " ", "\t"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.validate_display_name(value)

    def test_rejects_display_controls_and_bidi(self):
        for value in ("bad\nname", "bad\x1bname", "bad\x7fname", "bad\u202ename", "bad\u2066name"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.validate_display_name(value)

    def test_rejects_display_name_byte_overflow(self):
        with self.assertRaises(ValueError):
            packager.validate_display_name("\u00e9" * 65)

    def test_accepts_portable_paths(self):
        for value in ("fixture.exe", "docs/provider-guide.md", "weights/model_1.safetensors"):
            with self.subTest(value=value):
                self.assertEqual(packager.relative_name(value), value)

    def test_rejects_traversal_absolute_and_ads(self):
        for value in ("../bad", "a/../b", "/bad", "C:/bad", "a\\b", "a:stream", "a//b", "a/./b"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.relative_name(value)

    def test_rejects_hidden_and_device_paths(self):
        for value in (".hidden", "a/.hidden", "NUL", "con.txt", "aux.md", "COM0.exe", "lpt9", "clock", "CONIN"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.relative_name(value)

    def test_rejects_nonportable_and_trailing_path_characters(self):
        for value in ("a.", "a ", "a/b.", "a?b", "a b", "caf\u00e9", "a\nb"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.relative_name(value)

    def test_rejects_path_limits(self):
        for value in ("a" * 101, "/".join(["a"] * 13), "/".join(["a" * 90] * 3)):
            with self.subTest(value=value), self.assertRaises(ValueError):
                packager.relative_name(value)


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="aizen-packager-test-")
        self.addCleanup(self.fixture.cleanup)
        self.root = Path(self.fixture.name)
        self.binary = self.root / "fixture.bin"
        self.binary.write_bytes(b"Synthetic non-native packaging fixture; never executable.\n")
        self.license = self.root / "license-input"
        self.license.write_text("Synthetic license fixture; not a distribution.\n", encoding="utf-8")
        self.notice = self.root / "notice-input"
        self.notice.write_text("Synthetic notice fixture.\n", encoding="utf-8")

    def args(self, **changes):
        values = dict(id="example-memory", name="Independent packaging fixture", version="0.1.0",
                      target="x86_64-pc-windows-msvc", binary=self.binary, entrypoint="fixture.exe",
                      license=self.license, notice=self.notice, file=[], capability=["recall", "records"],
                      network=False, host_llm=False, output=self.root / "output")
        values.update(changes)
        return argparse.Namespace(**values)

    def rejected(self, **changes):
        with self.assertRaises(ValueError):
            packager.package(self.args(**changes))
        self.assertFalse((self.root / "output").exists(), "invalid package was published locally")
        self.assertFalse(list(self.root.glob(".memory-package-*")), "staging was leaked")

    def test_rejects_invalid_ids(self):
        for value in ("", "Bad", "9bad", "bad_", "bad-", "con", "a" * 65):
            with self.subTest(value=value):
                self.rejected(id=value)

    def test_rejects_unknown_target(self):
        self.rejected(target="x86_64-unknown-linux-gnu")

    def test_rejects_unknown_or_duplicate_capability(self):
        self.rejected(capability=["recall", "unknown"])
        self.rejected(capability=["recall", "recall"])

    def test_requires_matching_host_llm_permission(self):
        self.rejected(host_llm=True)
        self.rejected(capability=["host_llm"])

    def test_rejects_nested_or_nonexe_windows_entrypoint(self):
        self.rejected(entrypoint="bin/fixture.exe")
        self.rejected(entrypoint="fixture")

    def test_rejects_manifest_and_payload_case_collisions(self):
        self.rejected(entrypoint="LICENSE")
        self.rejected(file=[f"PLUGIN.JSON={self.notice}"])
        self.rejected(file=[f"license={self.notice}"])
        self.rejected(file=[f"fixture.EXE={self.notice}"])

    def test_rejects_file_directory_and_case_prefix_collisions(self):
        self.rejected(file=[f"LICENSE/extra={self.notice}"])
        self.rejected(file=[f"Docs/a={self.notice}", f"docs/b={self.license}"])
        self.rejected(file=[f"docs={self.notice}", f"docs/a={self.license}"])

    def test_rejects_hardlinked_inputs(self):
        link = self.root / "hardlink.bin"
        try:
            link.hardlink_to(self.binary)
        except OSError as error:
            self.skipTest(f"fixture filesystem does not support hardlinks: {error}")
        self.rejected(binary=link)

    def test_rejects_directory_payload(self):
        self.rejected(binary=self.root)

    def test_existing_output_is_never_overwritten(self):
        output = self.root / "output"
        output.mkdir()
        sentinel = output / "sentinel"
        sentinel.write_bytes(b"retain owned fixture bytes")
        with self.assertRaises(ValueError):
            packager.package(self.args())
        self.assertEqual(sentinel.read_bytes(), b"retain owned fixture bytes")
        self.assertEqual(list(output.iterdir()), [sentinel])

    def test_manifest_hashes_archive_and_determinism(self):
        extra = self.root / "guide-input"
        extra.write_text("Fixture-only guide.\n", encoding="utf-8")
        args = self.args(file=[f"docs/guide.md={extra}"])
        archive = packager.package(args)
        with zipfile.ZipFile(archive) as source:
            self.assertEqual(set(source.namelist()), {"fixture.exe", "LICENSE", "NOTICE", "plugin.json", "docs/guide.md"})
            manifest = json.loads(source.read("plugin.json"))
            self.assertEqual(manifest["id"], "example-memory")
            self.assertEqual(manifest["capabilities"], ["recall", "records"])
            self.assertEqual(manifest["permissions"], {"network": False, "host_llm": False})
            self.assertEqual(manifest["protocol"], {"major": 1, "min_minor": 0, "max_minor": 0})
            for item in manifest["files"]:
                data = source.read(item["path"])
                self.assertEqual(item["bytes"], len(data))
                self.assertEqual(item["sha256"], hashlib.sha256(data).hexdigest())
            for item in source.infolist():
                self.assertFalse(item.flag_bits & 8, "data descriptors are unsupported by installer")
                self.assertFalse(item.extra or item.comment)
                self.assertEqual(item.date_time, (2026, 1, 1, 0, 0, 0))
        self.assertEqual((args.output / "SHA256SUMS").read_text(encoding="ascii"),
                         f"{packager.digest(archive)}  {archive.name}\n")
        second = packager.package(self.args(file=[f"docs/guide.md={extra}"], output=self.root / "second"))
        self.assertEqual(archive.read_bytes(), second.read_bytes())


if __name__ == "__main__":
    unittest.main()
