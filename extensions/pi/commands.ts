import type { Connection, Bridge } from "./bridge.ts";

export const commandNames = ["connect", "model", "models", "commands", "compact", "run", "status", "native", "help"];
export const commandHelp = `Remote controls (Pi's own /model, /compact, /settings and /help stay local):
/tns connect — choose agent and SSH host
/tns model [ID] — select the remote model for subsequent prompts
/tns models — list the remote agent's model catalog
/tns commands — list remote commands supported here
/tns compact — compact remote history
/tns run /COMMAND [ARGS] — run a discovered Claude command or skill
/tns status — show endpoint, session ID and selected model override
/tns native — temporarily open the original Claude/Codex terminal UI
/tns help — this guide

Headless protocols do not expose every native UI feature. /tns native gives
access to the original commands, pickers, settings and interactive tools.`;

export type RemoteModel = { id: string; name: string };
export async function models(bridge: Bridge, kind: Connection["kind"]): Promise<RemoteModel[]> {
  const result: RemoteModel[] = [];
  const cursors = new Set<string>();
  let cursor: string | null = null;
  do {
    const response = await bridge.request("models", cursor) as {data?: unknown[]; nextCursor?: string | null} | unknown[];
    const values = kind === "claude" ? response : !Array.isArray(response) ? response?.data : undefined;
    if (!Array.isArray(values)) throw new Error("The remote agent did not return a model catalog. Use /tns model ID or /tns native.");
    for (const item of values) {
      const m = item as {value?: string; model?: string; id?: string; displayName?: string; display_name?: string; name?: string};
      const id = m.value || m.model || m.id;
      if (typeof id === "string") result.push({id, name:m.displayName || m.display_name || m.name || id});
    }
    cursor = kind === "codex" && !Array.isArray(response) ? response.nextCursor || null : null;
    if (cursor && cursors.has(cursor)) throw new Error("Remote model catalog repeated its pagination cursor.");
    if (cursor) cursors.add(cursor);
  } while (cursor);
  return result;
}

export async function commands(bridge: Bridge): Promise<string[]> {
  const response = await bridge.request("commands");
  if (!Array.isArray(response)) throw new Error("This Claude version did not return supported commands. Use /tns native.");
  return response.flatMap(item => {
    const name = typeof item === "string" ? item : item?.name;
    return typeof name === "string" ? [name.replace(/^\//, "")] : [];
  });
}

export function remoteCommand(input: string, available: string[]): string {
  const command = input.trim().replace(/^\//, "");
  const name = command.split(/\s/, 1)[0];
  if (!name || !available.includes(name)) throw new Error("Command is not advertised by this remote session. Use /tns commands or /tns native.");
  return `/${command}`;
}

// Fish interprets backslashes inside single quotes; isolate them and apostrophes
// in double-quoted segments that also preserve their bytes in bash and zsh.
const quote = (value: string) => `'${value.replace(/['\\]/g, ch => ch === "'" ? "'\"'\"'" : "'\"\\\\\"'")}'`;
export function nativeLaunch(c: Connection): {binary: string; args: string[]; cwd?: string} {
  const args = c.kind === "claude"
    ? ["claude", ...(c.resume ? ["--resume", c.resume] : [])]
    : ["codex", ...(c.resume ? ["resume", c.resume] : [])];
  if (c.model) args.push("--model", c.model);
  if (c.local) return {binary:args[0], args:args.slice(1), cwd:c.cwd || undefined};
  const command = `${c.cwd ? `cd -- ${quote(c.cwd)} && ` : ""}exec ${args.map(quote).join(" ")}`;
  return {binary:"ssh", args:["-t", "-o", "BatchMode=yes", "--", c.host, command]};
}
