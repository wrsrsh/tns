import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";
import { createInterface } from "node:readline";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

function binaryPath(): string {
  if (process.env.TNS_BIN) return process.env.TNS_BIN;
  for (const mode of ["debug", "release"]) {
    const path = fileURLToPath(new URL(`../../target/${mode}/tns`, import.meta.url));
    if (existsSync(path)) return path;
  }
  return "tns";
}

export interface Connection {
  kind: "claude" | "codex";
  host: string;
  local?: boolean;
  cwd?: string | null;
  resume?: string | null;
  extra?: string[];
  model?: string;
}

export type Event =
  | { type: "control_result"; data: { id: string; result: unknown; error: string | null } }
  | { type: "session_id" | "status" | "text_delta" | "error"; data: string }
  | { type: "turn_done"; data: string | null }
  | { type: "thinking" | "text_done" }
  | { type: "tool_start"; data: { id: string; name: string; summary: string } }
  | { type: "tool_done"; data: { id: string; ok: boolean; summary: string } }
  | { type: "permission"; data: { id: string; title: string; detail: string } };

export function connection(value: unknown): Connection {
  const c = value as Connection;
  if (!c || !["claude", "codex"].includes(c.kind) ||
      typeof c.host !== "string" || (!c.local && (!c.host || c.host.startsWith("-"))) ||
      (c.local !== undefined && typeof c.local !== "boolean") ||
      (c.cwd != null && typeof c.cwd !== "string") ||
      (c.resume != null && typeof c.resume !== "string") ||
      (c.model !== undefined && (typeof c.model !== "string" || !c.model.trim())) ||
      (c.extra !== undefined && (!Array.isArray(c.extra) || c.extra.some(x => typeof x !== "string")))) {
    throw new Error("Invalid TNS connection: choose claude or codex and a valid SSH host.");
  }
  return { ...c, extra: c.extra ? [...c.extra] : [] };
}

export function bridgeArgs(c: Connection): string[] {
  const args = ["agent", "--bridge", c.kind, ...(c.local ? ["--local"] : [c.host])];
  if (c.cwd) args.push("--cwd", c.cwd);
  if (c.resume) args.push("--resume", c.resume);
  if (c.extra?.length) args.push("--", ...c.extra);
  return args;
}

/** Persistent, shell-free local pipe to TNS. TNS owns the SSH process. */
export class Bridge extends EventEmitter {
  private child: ChildProcessWithoutNullStreams;
  private closed = false;
  private stderr = "";
  private nextId = 0;
  private pending = new Map<string, { resolve: (value: unknown) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }>();
  private stopped: Promise<void>;

  constructor(c: Connection, binary = binaryPath()) {
    super();
    this.child = spawn(binary, bridgeArgs(c), { stdio: "pipe" });
    this.stopped = new Promise(resolve => this.child.once("close", () => resolve()));
    const lines = createInterface({ input: this.child.stdout });
    lines.on("line", line => {
      if (this.closed) return;
      try {
        const event = JSON.parse(line) as Event;
        if (event.type === "control_result") {
          const pending = this.pending.get(event.data.id);
          if (pending) {
            clearTimeout(pending.timer);
            this.pending.delete(event.data.id);
            if (event.data.error) pending.reject(new Error(event.data.error));
            else pending.resolve(event.data.result);
          }
        }
        this.emit("event", event);
      }
      catch { this.fail("Invalid JSON from the TNS bridge."); }
    });
    this.child.stderr.on("data", data => { this.stderr = (this.stderr + data).slice(-2000); });
    this.child.stdin.on("error", error => this.fail(error.message));
    this.child.on("error", error => this.fail(error.message));
    this.child.on("close", () => {
      lines.close();
      this.fail(`TNS connection closed${this.stderr ? `: ${this.stderr.trim()}` : ""}`);
    });
  }

  private fail(message: string) {
    if (this.closed) return;
    this.emit("event", { type: "error", data: message } satisfies Event);
    this.close();
  }

  send(command: object) {
    if (this.closed) throw new Error("TNS connection is closed.");
    this.child.stdin.write(JSON.stringify(command) + "\n");
  }

  request(action: string, value: unknown = null, timeoutMs = 20000): Promise<unknown> {
    if (this.closed) return Promise.reject(new Error("TNS connection is closed."));
    const id = `pi-${++this.nextId}`;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Remote ${action} request timed out. Reconnect before retrying.`));
        this.fail("Remote control request timed out.");
      }, timeoutMs);
      this.pending.set(id, {resolve, reject, timer});
      try { this.send({type:"control", id, action, value}); }
      catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      }
    });
  }

  close(): Promise<void> {
    if (this.closed) return this.stopped;
    this.closed = true;
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("TNS connection closed before the control completed."));
    }
    this.pending.clear();
    this.removeAllListeners("event");
    // EOF lets the Rust bridge interrupt, kill and reap its agent/SSH child.
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill("SIGKILL"), 3000);
    timer.unref();
    this.child.once("close", () => clearTimeout(timer));
    return this.stopped;
  }
}
