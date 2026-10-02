#!/usr/bin/env python3
"""Read-only setup CLI tests. All commands and SSH hosts are local fakes."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

BIN = Path(os.environ.get("TNS_BIN", "target/debug/tns")).resolve()
FACTS = """TNS_SETUP_V1=1
OS=Linux
SHELL=/bin/bash
SHELL_OK=1
MOSH=/usr/bin/mosh-server
CHARMAP=UTF-8
UTF8=2
PKG=apt-get
CLAUDE=/usr/bin/claude
"""


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="tns-setup-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        (self.bin / "sh").symlink_to("/bin/sh")
        (self.root / ".ssh").mkdir()
        self.config = self.root / ".ssh/config"
        self.config.write_text("Host saved\n  HostName untouched.example\n")
        self.env = dict(os.environ, HOME=str(self.root), PATH=str(self.bin),
                        TEST_ROOT=str(self.root), SSH_FACTS=FACTS)
        self.script("mosh", "raise SystemExit(0)")
        self.script("locale", "print('UTF-8')")
        self.script("apt-get", "raise AssertionError('setup must not install anything')")
        self.script("ssh", """
root = Path(os.environ['TEST_ROOT'])
(root/'ssh-args').write_text(json.dumps(sys.argv[1:]))
(root/'ssh-script').write_text(sys.stdin.read())
if os.environ.get('SSH_FAIL'):
    print('Permission denied (publickey)', file=sys.stderr)
    raise SystemExit(255)
print(os.environ['SSH_FACTS'])
""")
        for tool in ["sudo", "ssh-keygen", "ssh-copy-id"]:
            self.script(tool, "raise AssertionError('setup must not change credentials or packages')")

    def script(self, name, body):
        p = self.bin / name
        p.write_text(f"#!{sys.executable}\nimport os, sys, json\nfrom pathlib import Path\n" + body)
        p.chmod(0o755)

    def run_setup(self, *args):
        result = subprocess.run([str(BIN), "setup", *args], stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, env=self.env, timeout=5)
        self.assertEqual(self.config.read_text(), "Host saved\n  HostName untouched.example\n")
        self.assertEqual(list((self.root / ".ssh").iterdir()), [self.config])
        self.assertNotIn("\x1b", result.stdout + result.stderr)
        return result

    def test_no_host_is_local_only_even_with_saved_hosts_and_eof(self):
        r = self.run_setup()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("Remote machine (not checked)", r.stdout)
        self.assertIn("A remote host still needs to be checked", r.stdout)
        self.assertFalse((self.root / "ssh-args").exists())

    def test_checks_both_machines_without_uploading_or_installing(self):
        r = self.run_setup("user@server")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("Local machine (this computer)", r.stdout)
        self.assertIn("Remote machine (user@server)", r.stdout)
        self.assertIn("Local and remote requirements passed", r.stdout)
        self.assertIn("UDP connectivity was not tested", r.stdout)
        args = json.loads((self.root / "ssh-args").read_text())
        self.assertIn("BatchMode=yes", args)
        self.assertIn("StrictHostKeyChecking=yes", args)
        self.assertIn("UpdateHostKeys=no", args)
        self.assertIn("ClearAllForwardings=yes", args)
        self.assertIn("ForwardAgent=no", args)
        self.assertIn("ControlPath=none", args)
        self.assertEqual(args[-4:], ["--", "user@server", "sh", "-s"])
        script = (self.root / "ssh-script").read_text()
        for mutation in ["mkdir", "chmod", "authorized_keys", "apt-get install", "cat >"]:
            self.assertNotIn(mutation, script)

    def test_local_mosh_client_is_optional(self):
        # tns has its own mosh client; only --mosh-client runs the program.
        (self.bin / "mosh").unlink()
        r = self.run_setup("server")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("no local mosh client: not required", r.stdout)
        self.assertNotIn("Run on this LOCAL machine", r.stdout)
        self.assertIn("Local and remote requirements passed", r.stdout)

    def test_missing_remote_mosh_has_remote_instructions(self):
        self.env["SSH_FACTS"] = FACTS.replace("MOSH=/usr/bin/mosh-server", "MOSH=")
        r = self.run_setup("server")
        self.assertEqual(r.returncode, 1)
        self.assertIn("Run on the REMOTE machine", r.stdout)
        self.assertIn("sudo apt-get install mosh", r.stdout)
        self.assertNotIn("requirements passed", r.stdout)

    def test_ssh_only_does_not_require_mosh_on_either_machine(self):
        (self.bin / "mosh").unlink()
        self.env["SSH_FACTS"] = FACTS.replace("MOSH=/usr/bin/mosh-server", "MOSH=")
        r = self.run_setup("--ssh", "server")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("tns --ssh server", r.stdout)
        self.assertNotIn("UDP connectivity", r.stdout)

    def test_failed_ssh_never_offers_password_only_as_working_setup(self):
        self.env["SSH_FAIL"] = "1"
        r = self.run_setup("server")
        self.assertEqual(r.returncode, 1)
        self.assertIn("Password-only login does not work", r.stdout)
        self.assertIn("ssh -o BatchMode=yes server true", r.stdout)
        self.assertNotIn("requirements passed", r.stdout)

    def test_incomplete_report_and_missing_locale_are_failures(self):
        for facts in ["", "a login banner", FACTS.replace("UTF8=2", "UTF8=0").replace("CHARMAP=UTF-8", "CHARMAP=ASCII")]:
            with self.subTest(facts=facts):
                self.env["SSH_FACTS"] = facts
                r = self.run_setup("server")
                self.assertEqual(r.returncode, 1)
                self.assertNotIn("requirements passed", r.stdout)

    def test_claude_is_optional_and_authentication_is_not_assumed(self):
        self.env["SSH_FACTS"] = FACTS.replace("CLAUDE=/usr/bin/claude", "CLAUDE=")
        r = self.run_setup("server")
        self.assertEqual(r.returncode, 0)
        self.assertIn("Claude Code is optional", r.stdout)

    def test_help_and_bad_arguments_do_not_probe_a_host(self):
        self.assertEqual(self.run_setup("--help").returncode, 0)
        for args in [("--local", "server"), ("a", "b"), ("--install",), ("x;touch-file",), ("-oProxyCommand=x",)]:
            self.assertEqual(self.run_setup(*args).returncode, 2)
        self.assertFalse((self.root / "ssh-args").exists())


if __name__ == "__main__":
    unittest.main()
