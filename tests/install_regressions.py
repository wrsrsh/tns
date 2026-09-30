#!/usr/bin/env python3
"""Installer tests with fake package managers. No network or system changes."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "install.sh"
TOOL = r"""
root = Path(os.environ['TEST_ROOT'])
name = Path(sys.argv[0]).name
args = sys.argv[1:]
if name == 'uname':
    print(os.environ.get('TEST_OS', 'Linux'))
elif name == 'id':
    print('0')
elif name == 'locale':
    print('C\nC.utf8' if '-a' in args else os.environ.get('TEST_CHARMAP', 'UTF-8'))
elif name in ('brew', 'apt-get', 'cargo', 'sudo'):
    with (root/'calls').open('a') as f:
        f.write(json.dumps([name, *args])+'\n')
    if name == 'brew' and args[0] == 'list':
        raise SystemExit(0 if os.environ.get('BREW_INSTALLED') else 1)
    if name == 'brew' and args[0] == '--prefix':
        print(root)
        raise SystemExit
    if os.environ.get('FAIL_TOOL') == name:
        raise SystemExit(17)
    if name in ('brew', 'apt-get') and args[0] != 'update':
        for binary in ('mosh', 'mosh-server'):
            p=root/'bin'/binary
            p.write_text('#!/bin/sh\nexit 0\n')
            p.chmod(0o755)
    if name == 'brew' and any('tns' in arg for arg in args):
        p=root/'bin/tns'
        p.write_text('#!/bin/sh\necho \"tns 0.6.1\"\n')
        p.chmod(0o755)
    if name == 'cargo':
        p=Path(args[args.index('--root')+1])/'bin/tns'
        p.parent.mkdir(parents=True,exist_ok=True)
        p.write_text('#!/bin/sh\necho \"tns 0.6.1\"\n')
        p.chmod(0o755)
else:
    raise SystemExit(0)
"""


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="tns-install-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.env = dict(os.environ, HOME=str(self.root), PATH=str(self.bin),
                        TEST_ROOT=str(self.root), CARGO_HOME=str(self.root / "cargo"))
        for var in ["TNS_VERSION", "CARGO_INSTALL_ROOT", "FAIL_TOOL", "BREW_INSTALLED"]:
            self.env.pop(var, None)
        for name in ["cat", "grep", "dirname"]:
            (self.bin / name).symlink_to(shutil.which(name))
        for name in ["uname", "id", "locale", "ssh"]:
            self.tool(name)

    def tool(self, name):
        p = self.bin / name
        p.write_text(f"#!{sys.executable}\nimport os, sys, json\nfrom pathlib import Path\n" + TOOL)
        p.chmod(0o755)

    def calls(self):
        path = self.root / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def run_installer(self, *args):
        return subprocess.run(["/bin/sh", str(INSTALLER), *args], stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, env=self.env, timeout=5)

    def test_role_is_required_before_any_side_effect(self):
        self.assertEqual(self.run_installer().returncode, 2)
        self.assertEqual(self.run_installer("local", "remote").returncode, 2)
        self.assertEqual(self.run_installer("--help").returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_remote_installs_only_mosh_without_a_rust_toolchain(self):
        self.tool("apt-get")
        r = self.run_installer("remote")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.calls(), [["apt-get", "update"], ["apt-get", "install", "-y", "mosh"]])
        self.assertFalse((self.bin / "tns").exists())
        self.assertIn("LOCAL machine", r.stdout)
        self.assertIn("tns and Rust are not needed", r.stdout)

    def test_remote_is_idempotent_and_check_mode_never_installs(self):
        self.tool("apt-get")
        self.assertEqual(self.run_installer("remote", "--check").returncode, 1)
        self.assertEqual(self.calls(), [])
        self.tool("mosh-server")
        self.assertEqual(self.run_installer("remote").returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_package_manager_failure_is_not_swallowed(self):
        self.tool("apt-get")
        self.env["FAIL_TOOL"] = "apt-get"
        r = self.run_installer("remote")
        self.assertEqual(r.returncode, 17)
        self.assertEqual(self.calls(), [["apt-get", "update"]])
        self.assertNotIn("Next, on your LOCAL", r.stdout)

    def test_homebrew_installs_or_upgrades_and_propagates_errors(self):
        self.tool("brew")
        self.env["TEST_OS"] = "Darwin"
        r = self.run_installer("local")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn(["brew", "install", "wrsrsh/tap/tns"], self.calls())
        self.env["BREW_INSTALLED"] = "1"
        self.env["FAIL_TOOL"] = "brew"
        r = self.run_installer("local")
        self.assertEqual(r.returncode, 17)
        self.assertNotIn("Installed:", r.stdout)

    def test_existing_tns_without_homebrew_does_not_fake_a_macos_upgrade(self):
        self.env["TEST_OS"] = "Darwin"
        self.tool("tns")
        r = self.run_installer("local")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Install Homebrew", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_linux_requires_build_tools_before_changing_packages(self):
        self.tool("apt-get")
        r = self.run_installer("local")
        self.assertEqual(r.returncode, 1)
        self.assertIn("Rust toolchain", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_linux_source_install_uses_a_release_and_reports_path(self):
        for tool in ["cargo", "cc", "apt-get"]:
            self.tool(tool)
        self.env["PATH"] += os.pathsep + str(self.root / "cargo/bin")
        r = self.run_installer("local")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        cargo = next(call for call in self.calls() if call[0] == "cargo")
        self.assertIn("--locked", cargo)
        self.assertEqual(cargo[cargo.index("--tag")+1], "v0.6.1")
        self.assertIn("Remote machine: not checked", r.stdout)

    def test_missing_cargo_path_is_an_actionable_failure(self):
        for tool in ["cargo", "cc", "mosh"]:
            self.tool(tool)
        r = self.run_installer("local")
        self.assertEqual(r.returncode, 1)
        self.assertIn("Your shell does not resolve tns", r.stdout)
        self.assertIn("export PATH=", r.stdout)

    def test_local_check_is_non_mutating_and_fails_missing_requirements(self):
        self.tool("brew")
        r = self.run_installer("local", "--check")
        self.assertEqual(r.returncode, 1)
        self.assertIn("[missing] tns", r.stdout)
        self.assertEqual(self.calls(), [])

    def test_installer_default_version_matches_the_package(self):
        version = re.search(r'^version = \"([^\"]+)\"', (ROOT / "Cargo.toml").read_text(), re.M).group(1)
        self.assertIn("TNS_VERSION=${TNS_VERSION:-" + version + "}", INSTALLER.read_text())


if __name__ == "__main__":
    unittest.main()
