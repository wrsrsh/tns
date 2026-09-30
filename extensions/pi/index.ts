import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { createAssistantMessageEventStream, type AssistantMessage } from "@earendil-works/pi-ai";
import { Bridge, connection, type Connection, type Event } from "./bridge.ts";
import { spawn } from "node:child_process";
import { commandNames, commandHelp, models, commands, remoteCommand, nativeLaunch, type RemoteModel } from "./commands.ts";

export default function tns(pi: ExtensionAPI) {
  // Source checkouts and installed Pi packages can both discover this extension.
  // Use the session's shared bus, not process globals, to register only one copy.
  const claim = { registered: false };
  pi.events.emit("tns:extension-claim", claim);
  if (claim.registered) return;
  pi.events.on("tns:extension-claim", (value: unknown) => {
    (value as typeof claim).registered = true;
  });
  let config: Connection | undefined;
  let ui: ExtensionContext | undefined;
  let bridge: Bridge | undefined;
  let active = false;
  let commandBusy = false;
  let knownModels: RemoteModel[] = [];
  let knownCommands: string[] = [];
  let savedTools: string[] | undefined;
  const configured = process.env.TNS_AGENT_CONFIG;
  if (configured) config = connection(JSON.parse(configured));
  let nextConfig = config;

  const close = () => { bridge?.close(); bridge = undefined; };
  const persist = () => { if (config) pi.appendEntry("tns-connection", { ...config }); };
  const status = (text = "connected") => ui?.ui.setStatus("tns", config
    ? `${config.kind} @ ${config.local ? "local" : config.host} · ${text}` : undefined);
  const ensureBridge = async () => {
    if (!config) throw new Error("Use /tns connect first.");
    if (!bridge) {
      const created = new Bridge(config);
      bridge = created;
      created.on("event", (event: Event) => {
        if (bridge !== created) return;
        if (event.type === "session_id" && config) { config.resume = event.data; persist(); }
        if (event.type === "error" && !active) { close(); status("disconnected"); }
      });
      if (config.model) {
        try { await created.request("model", config.model); }
        catch (error) { close(); throw error; }
      }
    }
    return bridge!;
  };
  const show = (content: string) => pi.sendMessage({customType:"tns-control", content, display:true}, {triggerTurn:false});

  pi.registerProvider("tns", {
    baseUrl: "ssh://tns", apiKey: "remote-agent-auth", api: "tns-agent",
    models: [{ id: "remote", name: "TNS remote agent", reasoning: false, input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 200000, maxTokens: 32000 }],
    streamSimple(model, context, options) {
      const stream = createAssistantMessageEventStream();
      const block = { type: "text" as const, text: "" };
      const message: AssistantMessage = {
        role: "assistant", content: [block], api: model.api, provider: model.provider,
        model: model.id, timestamp: Date.now(), stopReason: "pending",
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
          cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
      };
      let finished = false;
      let started = false;
      let listener: ((event: Event) => void) | undefined;
      let current: Bridge | undefined;
      const dialogs = new AbortController();
      const finish = (error?: string, aborted = false) => {
        if (finished) return;
        finished = true;
        active = false;
        dialogs.abort();
        options?.signal?.removeEventListener("abort", abort);
        if (listener) current?.off("event", listener);
        if (started) stream.push({ type: "text_end", contentIndex: 0, content: block.text, partial: message });
        if (error) {
          message.stopReason = aborted ? "aborted" : "error";
          message.errorMessage = error;
          stream.push({ type: "error", reason: message.stopReason, error: message });
          close();
        } else {
          message.stopReason = "stop";
          stream.push({ type: "done", reason: "stop", message });
        }
        stream.end();
        status(error ? "disconnected; next message resumes" : "ready");
      };
      const abort = () => {
        try { current?.send({ type: "interrupt" }); } catch { /* connection already closed */ }
        finish("Remote turn interrupted. The next message resumes the remote session.", true);
      };
      const append = (text: string) => {
        block.text += text;
        stream.push({ type: "text_delta", contentIndex: 0, delta: text, partial: message });
      };
      void (async () => {
        try {
          if (active || commandBusy) throw new Error("A remote turn or command is already running.");
          if (!config) throw new Error("Use /tns to connect a remote Claude or Codex agent first.");
          const user = context.messages.findLast(m => m.role === "user");
          if (!user || user.role !== "user") throw new Error("No user message to send.");
          if (Array.isArray(user.content) && user.content.some(b => b.type !== "text")) {
            throw new Error("TNS currently accepts text only; attachments are not forwarded.");
          }
          const text = typeof user.content === "string" ? user.content : user.content.map(b => b.type === "text" ? b.text : "").join("\n");
          const payload = await options?.onPayload?.({ type: "send", text }, model) ?? { type: "send", text };
          if (!payload || typeof payload !== "object" || !("type" in payload) || payload.type !== "send" ||
              !("text" in payload) || typeof payload.text !== "string") throw new Error("Invalid TNS prompt payload.");
          if (options?.signal?.aborted) { abort(); return; }
          active = true;
          options?.signal?.addEventListener("abort", abort, { once: true });
          current = await ensureBridge();
          if (finished) { close(); return; }
          listener = (event: Event) => {
            if (finished) return;
            switch (event.type) {
              case "text_delta": append(event.data); break;
              case "text_done": if (block.text && !block.text.endsWith("\n\n")) append("\n\n"); break;
              case "thinking": status("thinking"); break;
              case "status": status(event.data); break;
              case "tool_start": append(`\n\n> ${event.data.name}: ${event.data.summary}\n\n`); break;
              case "tool_done": if (event.data.summary || !event.data.ok) append(`\n\n> ${event.data.ok ? "Done" : "Failed"}: ${event.data.summary}\n\n`); break;
              case "permission": {
                const request = event.data;
                void (async () => {
                  const choice = ui?.hasUI ? await ui.ui.select(`${request.title}\n${request.detail}`,
                    ["Deny", "Allow once", "Allow for session"], { signal: dialogs.signal }) : undefined;
                  if (!finished) current?.send({ type: "answer", id: request.id,
                    reply: choice === "Allow once" ? "allow" : choice === "Allow for session" ? "allow_always" : "deny" });
                })().catch(error => finish(String(error)));
                break;
              }
              case "turn_done": finish(); if (event.data) status(event.data); break;
              case "error": finish(event.data); break;
            }
          };
          current.on("event", listener);
          stream.push({ type: "start", partial: message });
          stream.push({ type: "text_start", contentIndex: 0, partial: message });
          started = true;
          status("working");
          current.send(payload);
        } catch (error) { finish(error instanceof Error ? error.message : String(error)); }
      })();
      return stream;
    },
  });

  const syncTools = (ctx: ExtensionContext) => {
    if (ctx.model?.provider === "tns") {
      savedTools ??= pi.getActiveTools();
      pi.setActiveTools([]);
    } else if (savedTools) {
      pi.setActiveTools(savedTools);
      savedTools = undefined;
    }
  };
  pi.on("model_select", (_event, ctx) => { ui = ctx; syncTools(ctx); });
  pi.on("session_start", async (_event, ctx) => {
    close();
    knownModels = [];
    knownCommands = [];
    ui = ctx;
    const entry = ctx.sessionManager.getBranch().findLast(e => e.type === "custom" && e.customType === "tns-connection");
    config = entry?.type === "custom" ? connection(entry.data) : nextConfig;
    nextConfig = config ? { ...config, resume: undefined } : undefined;
    if (config) persist();
    syncTools(ctx);
    status("ready");
  });
  pi.on("session_shutdown", close);
  // Remote agents own their history. Pi branches/compaction cannot rewind it.
  const blockHistoryRewrite = (_event: unknown, ctx: ExtensionContext) => {
      if (ctx.model?.provider !== "tns") return;
      ctx.ui.notify("The remote agent owns its history. Use /tns compact for remote compaction, /new for a fresh session, or /tns native for native history controls.", "warning");
      return { cancel: true };
  };
  pi.on("session_before_fork", blockHistoryRewrite);
  pi.on("session_before_tree", blockHistoryRewrite);
  pi.on("session_before_compact", blockHistoryRewrite);
  pi.on("session_before_switch", (event) => {
    if (active || commandBusy) return { cancel: true };
    close();
    // A new Pi session must not inherit the old remote conversation.
    if (event.reason === "new" && config) nextConfig = { ...config, resume: undefined };
  });
  pi.registerCommand("tns", {
    description: "Remote agent controls: connect, model, commands, compact, native, help",
    getArgumentCompletions: prefix => {
      const choices = prefix.startsWith("model ") ? knownModels.map(m => `model ${m.id}`)
        : prefix.startsWith("run ") ? knownCommands.map(name => `run /${name}`) : commandNames;
      return choices.filter(value => value.startsWith(prefix)).map(value => ({value, label:value}));
    },
    handler: async (args, ctx) => {
      const [action = "connect"] = args.trim().split(/\s+/).filter(Boolean);
      const value = args.trim().slice(action.length).trim();
      ui = ctx;
      if (action === "help") { show(commandHelp); return; }
      if (active || commandBusy) { ctx.ui.notify("Wait for the remote turn or command to finish, or cancel it first.", "warning"); return; }
      if (action !== "connect") {
        if (!config) { ctx.ui.notify("Use /tns connect first.", "warning"); return; }
        commandBusy = true;
        let prompt: string | undefined;
        try {
          if (action === "status") {
            show(`${config.kind} @ ${config.local ? "local" : config.host}\nDirectory: ${config.cwd || "remote default"}\nRemote session: ${config.resume || "not started"}\nModel override: ${config.model || "remote default"}`);
          } else if (action === "models" || action === "model") {
            const remote = await ensureBridge();
            let selected = value;
            if (action === "models" || !selected) {
              knownModels = await models(remote, config.kind);
              if (action === "models") show(knownModels.map(m => `${m.id} — ${m.name}`).join("\n") || "No models advertised. Use /tns model ID or /tns native.");
              else if (ctx.hasUI) {
                const labels = knownModels.map(m => `${m.id} — ${m.name}`);
                const choice = await ctx.ui.select(`Model on ${config.kind} @ ${config.host || "local"}`, labels);
                selected = choice ? knownModels[labels.indexOf(choice)]?.id : "";
              } else throw new Error("Use /tns model ID without an interactive UI.");
            }
            if (action === "model" && selected) {
              await remote.request("model", selected);
              config.model = selected;
              nextConfig = { ...config, resume:undefined };
              persist();
              status(`model: ${selected}`);
              show(`Remote ${config.kind} model set to ${selected} for subsequent prompts. Pi's /model remains local.`);
            }
          } else if (action === "commands") {
            if (config.kind === "claude") {
              knownCommands = await commands(await ensureBridge());
              show(knownCommands.map(name => `/tns run /${name}`).join("\n") || "No headless commands advertised.");
            } else show("Codex controls in Pi: /tns model, /tns models, /tns compact. For Codex's native slash commands, use /tns native.");
          } else if (action === "compact" || action === "run") {
            if (ctx.model?.provider !== "tns") throw new Error("Select tns/remote with Pi's /model before sending remote commands.");
            if (action === "compact") {
              if (value && config.kind === "codex") throw new Error("Codex compaction does not accept instructions through this protocol.");
              prompt = `/compact${value ? ` ${value}` : ""}`;
            } else {
              if (config.kind !== "claude") throw new Error("Codex slash commands belong to its TUI. Use /tns native.");
              knownCommands = await commands(await ensureBridge());
              prompt = remoteCommand(value, knownCommands);
            }
          } else if (action === "native") {
            if (ctx.mode !== "tui") throw new Error("/tns native requires an interactive Pi terminal.");
            const launch = nativeLaunch(config);
            const previous = bridge;
            bridge = undefined;
            await previous?.close();
            const code = await ctx.ui.custom<number>((tui, _theme, _keys, done) => {
              let ended = false;
              const finish = (code: number) => {
                if (ended) return;
                ended = true;
                tui.start();
                tui.requestRender(true);
                done(code);
              };
              tui.stop();
              if (process.stdout.isTTY) process.stdout.write("\x1b[2J\x1b[H");
              try {
                const child = spawn(launch.binary, launch.args, {cwd:launch.cwd, stdio:"inherit"});
                child.once("error", error => { process.stderr.write(`${error.message}\n`); finish(1); });
                child.once("close", code => finish(code ?? 1));
              } catch { finish(1); }
              return {render:() => [], invalidate:() => {}};
            });
            // Let any model changes made in the native session survive resumption.
            config.model = undefined;
            nextConfig = {...config, resume:undefined};
            persist();
            show(`Native ${config.kind} exited (${code}). ${config.resume ? "The next Pi prompt resumes the remote session; native turns remain in remote history." : "That native session was separate; use its ID with tns agent --resume to continue it here."}`);
          } else throw new Error(`Unknown /tns command: ${action}. Use /tns help.`);
        } catch (error) {
          ctx.ui.notify(error instanceof Error ? error.message : String(error), "error");
        } finally { commandBusy = false; }
        if (prompt) pi.sendUserMessage(prompt, {expandPromptTemplates:false});
        return;
      }
      if (!ctx.hasUI || active) { ctx.ui.notify("Connect from an idle interactive Pi session.", "warning"); return; }
      const kind = await ctx.ui.select("Remote agent", ["claude", "codex"]);
      if (!kind) return;
      const host = await ctx.ui.input("SSH host (or --local)", config?.host || "user@server");
      if (!host) return;
      const cwd = await ctx.ui.input("Working directory on that machine (blank for default)", config?.cwd || "");
      if (cwd === undefined) return;
      const next = connection({ kind, host: host === "--local" ? "" : host, local: host === "--local", cwd });
      const model = ctx.modelRegistry.find("tns", "remote");
      if (!model || !await pi.setModel(model)) throw new Error("Could not select the TNS provider.");
      close();
      knownModels = [];
      knownCommands = [];
      config = next;
      nextConfig = { ...next, resume: undefined };
      ui = ctx;
      persist();
      savedTools ??= pi.getActiveTools();
      pi.setActiveTools([]);
      status("ready");
    },
  });
}
