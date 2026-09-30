import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { nativeLaunch } from "../extensions/pi/commands.ts";

const shells = [
  { name: "bash", flags: ["--noprofile", "--norc"] },
  { name: "zsh", flags: ["-f"] },
  { name: "fish", flags: ["--no-config"] },
];
const values = [
  "plain", "/path/with spaces/", "雪/é/e\u0301/🦀", "'", '"', "\\", "\\\\",
  "\\'", "'\\", "ends in\\", "line one\nline two\n", "\\\n'\n\\",
  "$HOME ${HOME}", "$(printf INJECTED)", "(printf INJECTED)", "`printf INJECTED`",
  "'; printf INJECTED; #", '"; printf INJECTED; #',
  "* ? [a-z] {a,b} ~ % ; & | < > # !", "--leading-dash", "tab\tcarriage\rreturn",
  "\\\"$`'\n/雪\\'",
];

for (const { name, flags } of shells) {
  for (const kind of ["claude", "codex"] as const) {
    test(`${kind}: nativeLaunch preserves cwd and argument bytes in ${name}`, t => {
      const available = spawnSync(name, [...flags, "-c", "printf '%s' available"]);
      if (available.error && "code" in available.error && available.error.code === "ENOENT") {
        t.skip(`${name} not installed`);
        return;
      }
      assert.ifError(available.error);
      assert.equal(available.status, 0, available.stderr?.toString());

      const root = realpathSync(mkdtempSync(join(tmpdir(), "tns-shell-quoting-")));
      t.after(() => rmSync(root, { recursive: true, force: true }));
      const cwd = join(root, "cwd\\backslash'quote\n雪");
      mkdirSync(cwd);
      // No agent or SSH is launched: the fake native binary only prints its cwd/argv.
      writeFileSync(join(root, kind), `#!/bin/sh\nprintf '%s\\0' "$PWD" '${kind}' "$@"\n`, { mode: 0o755 });
      for (const value of values) {
        const launch = nativeLaunch({ kind, host: "unused-host", cwd, resume: value, model: value });
        assert.equal(launch.binary, "ssh");
        assert.deepEqual(launch.args.slice(0, -1), ["-t", "-o", "BatchMode=yes", "--", "unused-host"]);
        const result = spawnSync(name, [...flags, "-c", launch.args.at(-1)!], {
          env: { ...process.env, PATH: `${root}:${process.env.PATH}` },
        });
        assert.ifError(result.error);
        assert.equal(result.status, 0, `${name}: ${result.stderr?.toString()}`);
        const args = kind === "claude" ? ["--resume", value] : ["resume", value];
        const expected = Buffer.from([cwd, kind, ...args, "--model", value, ""].join("\0"));
        assert.deepEqual(result.stdout, expected, `${name}: changed bytes for ${JSON.stringify(value)}`);
        assert.equal(result.stderr.length, 0, result.stderr.toString());
      }
    });
  }
}

for (const kind of ["claude", "codex"] as const) {
  test(`${kind}: local nativeLaunch remains shell-free`, () => {
    const value = "雪/\\'\"\n$HOME";
    assert.deepEqual(nativeLaunch({ kind, local: true, host: "", cwd: value, resume: value, model: value }), {
      binary: kind,
      args: [...(kind === "claude" ? ["--resume", value] : ["resume", value]), "--model", value],
      cwd: value,
    });
  });
}
