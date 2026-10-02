"""发布门禁与说明范围回归；不访问 registry，不创建 tag 或 Release。"""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import notes


class ReleaseNotesTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        description = self.root / "docs/releases/v0.1.0.md"
        description.parent.mkdir(parents=True)
        description.write_text("## Version description\nPublic API changes\n")
        self.root_patch = patch.object(notes, "ROOT", self.root)
        self.root_patch.start()
        self.package = {
            "name": "waybill",
            "version": "0.1.0",
            "publish": ["crates-io"],
            "dependencies": [],
        }
        self.calls = []
        self.previous_tags = "v0.1.0\nv0.0.9"
        self.body = "User release description"

    def tearDown(self):
        self.root_patch.stop()
        self.directory.cleanup()

    def command(self, *args):
        self.calls.append(args)
        if args[0] == "cargo":
            return json.dumps({"packages": [self.package, {"publish": []}]})
        if args[:2] == ("git", "rev-parse"):
            return "a" * 40
        if args[:2] == ("git", "tag"):
            return self.previous_tags
        if args[:2] == ("git", "log"):
            return "b" * 40 + "\tfeat: [a] public API"
        if args[0] == "gh":
            return json.dumps({"body": self.body})
        raise AssertionError(args)

    def test_tag_and_dependency_version_gate(self):
        with patch.object(notes, "command", self.command):
            notes.check("v0.1.0", True)
            with self.assertRaises(ValueError):
                notes.check("../../invalid")
            self.package["version"] = "0.2.0"
            with self.assertRaises(ValueError):
                notes.check("v0.1.0")
            self.package["version"] = "0.1.0"
            self.package["dependencies"] = [
                {"name": "service", "path": "/service", "req": "*"}
            ]
            with self.assertRaises(ValueError):
                notes.check("v0.1.0")

    def test_previous_tag_range_and_rerun_preserve_user_body(self):
        output = self.root / "notes.md"
        with patch.object(notes, "command", self.command):
            notes.render("v0.1.0", output)
            first = output.read_text()
            self.body = first
            notes.render("v0.1.0", output)
        self.assertEqual(first, output.read_text())
        self.assertIn("User release description", first)
        self.assertIn("Public API changes", first)
        self.assertIn(("git", "log", "--format=%H%x09%s", "v0.0.9..v0.1.0"), self.calls)
        self.assertIn(r"\[a\] public API", first)

    def test_initial_release_includes_complete_history(self):
        self.previous_tags = "v0.1.0"
        with patch.object(notes, "command", self.command):
            notes.render("v0.1.0", self.root / "notes.md")
        self.assertIn(("git", "log", "--format=%H%x09%s", "v0.1.0"), self.calls)

    def test_wrong_checkout_and_malformed_generated_block_fail_closed(self):
        def mismatch(*args):
            if args == ("git", "rev-parse", "HEAD"):
                return "c" * 40
            return self.command(*args)

        with patch.object(notes, "command", mismatch), self.assertRaises(ValueError):
            notes.check("v0.1.0", True)
        self.body = notes.START
        with (
            patch.object(notes, "command", self.command),
            self.assertRaises(ValueError),
        ):
            notes.render("v0.1.0", self.root / "notes.md")


if __name__ == "__main__":
    unittest.main()
