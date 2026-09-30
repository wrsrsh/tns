import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtempSync, writeFileSync, readFileSync, rmSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { createInterface } from "node:readline";
import { createEventBus, DefaultResourceLoader, type ExtensionAPI, type ExtensionContext, type ProviderConfig } from "@earendil-works/pi-coding-agent";
import { normalizeContext, type Model, type Api, type AssistantMessageEvent } from "@earendil-works/pi-ai";
import extension from "../extensions/pi/index.ts";
import { Bridge, connection, bridgeArgs, type Connection } from "../extensions/pi/bridge.ts";
import { nativeLaunch } from "../extensions/pi/commands.ts";

const dir = mkdtempSync(join(tmpdir(), "tns-pi-"));
const log = join(dir, "agent.log");
const originalPath = process.env.PATH;
process.env.PATH = `${dir}:${originalPath}`;
process.env.TNS_BIN = resolve("target/debug/tns");
process.env.TNS_TEST_LOG = log;
after(() => { process.env.PATH = originalPath; rmSync(dir, { recursive: true, force: true }); });

const fixture = `#!/usr/bin/env node
const {createInterface}=require('node:readline');
const {appendFileSync}=require('node:fs');
const codex=process.argv[1].endsWith('codex');
const log=x=>appendFileSync(process.env.TNS_TEST_LOG,JSON.stringify(x)+'\\n');
log({argv:process.argv.slice(2),kind:codex?'codex':'claude',cwd:process.cwd()});
if(!process.argv.includes('-p')&&!process.argv.includes('app-server'))process.exit(0);
const emit=x=>process.stdout.write(JSON.stringify(x)+'\\n');
let turn=0;
const done=text=>{
 if(codex){emit({method:'item/agentMessage/delta',params:{delta:text}});emit({method:'turn/completed',params:{turn:{status:'completed'}}});}
 else {emit({type:'stream_event',event:{type:'content_block_delta',delta:{type:'text_delta',text}}});emit({type:'result',is_error:false});}
};
if(!codex) emit({type:'system',subtype:'init',session_id:'claude-saved'});
createInterface({input:process.stdin}).on('line',line=>{
 const v=JSON.parse(line);log(v);
 if(codex && v.method==='initialize')emit({id:v.id,result:{}});
 else if(codex && v.method==='model/list')emit({id:v.id,result:{data:[{model:v.params.cursor?'model-b':'model-a',displayName:'Remote model'}],nextCursor:v.params.cursor?null:'page-2'}});
 else if(codex && v.method==='thread/compact/start'){
   emit({id:v.id,result:{}});emit({method:'turn/started',params:{turn:{id:'compact-turn'}}});done('compacted');
 }
 else if(!codex && v.type==='control_request' && ['initialize','set_model'].includes(v.request.subtype)){
   if(v.request.model==='bad')emit({type:'control_response',response:{subtype:'error',request_id:v.request_id,error:'unknown model'}});
   else emit({type:'control_response',response:{subtype:'success',request_id:v.request_id,response:v.request.subtype==='initialize'?{models:[{value:'model-a',displayName:'Remote model'}],commands:[{name:'compact'},{name:'context'},{name:'custom-skill'}]}:{}}});
 }
 else if(codex && ['thread/start','thread/resume'].includes(v.method))emit({id:v.id,result:{thread:{id:'codex-saved'}}});
 else if(v.method==='turn/start'||v.type==='user'){
   const text=codex?v.params.input[0].text:v.message.content[0].text;
   if(codex)emit({id:v.id,result:{turn:{id:'turn-'+(++turn)}}});
   if(text==='hang')return;
   if(!codex && text==='/context'){emit({type:'result',result:'context from remote',is_error:false});return;}
   if(text==='drop'){process.exit(1);return;}
   if(text==='permission'){
     if(codex)emit({id:99,method:'item/commandExecution/requestApproval',params:{command:'echo safe'}});
     else emit({type:'control_request',request_id:'approval',request:{subtype:'can_use_tool',tool_name:'Bash',input:{command:'echo safe'}}});
   }else if(text==='tool'){
     if(codex){emit({method:'item/started',params:{item:{id:'tool',type:'commandExecution',command:'echo safe'}}});emit({method:'item/completed',params:{item:{id:'tool',type:'commandExecution',status:'completed',aggregatedOutput:'safe'}}});}
     else {emit({type:'assistant',message:{content:[{type:'tool_use',id:'tool',name:'Bash',input:{command:'echo safe'}}]}});emit({type:'user',message:{content:[{type:'tool_result',tool_use_id:'tool',content:'safe'}]}});}
     done('tool complete');
   }else done('reply: '+text);
 }else if(v.type==='control_response'||v.id===99)done('permission answered');
});
`;
for (const name of ["claude", "codex"]) writeFileSync(join(dir, name), fixture, { mode: 0o755 });
writeFileSync(join(dir, "ssh"), '#!/bin/sh\nfor last do :; done\nexec sh -c "$last"\n', { mode: 0o755 });

function harness(kind: "claude" | "codex", remote = false, overrides: Partial<Connection> = {}) {
  process.env.TNS_AGENT_CONFIG = JSON.stringify({kind, host: remote ? "fake-host" : "", local: !remote, cwd: dir, ...overrides});
  const hooks = new Map<string, Function>();
  const registered = new Map<string, {handler: Function; getArgumentCompletions?: Function}>();
  const notices: string[] = [];
  const messages: string[] = [];
  const prompts: {content:string; options:unknown}[] = [];
  const terminal: string[] = [];
  const entries: Array<{type: string; customType: string; data: unknown}> = [];
  let provider!: ProviderConfig;
  let tools = ["read", "bash"];
  let choice: string | undefined = "Allow once";
  const model = { provider: "tns", id: "remote", api: "tns-agent" } as Model<Api>;
  const ctx = {
    model, hasUI: true, mode:"tui",
    ui: { setStatus() {}, notify: (text:string) => notices.push(text), select: async () => choice,
      custom: (factory: Function) => new Promise(resolve => factory({stop:()=>terminal.push("stop"),start:()=>terminal.push("start"),requestRender(){}},null,null,resolve)) },
    sessionManager: { getBranch: () => entries },
  } as unknown as ExtensionContext;
  extension({
    events: createEventBus(),
    registerProvider: (_name: string, value: ProviderConfig) => { provider = value; },
    on: (name: string, handler: Function) => hooks.set(name, handler),
    registerCommand: (name:string, command:{handler:Function}) => registered.set(name,command),
    sendMessage: (message:{content:string}) => messages.push(message.content),
    sendUserMessage: (content:string, options:unknown) => prompts.push({content,options}),
    appendEntry: (customType: string, data: unknown) => entries.push({type:"custom",customType,data}),
    getActiveTools: () => tools,
    setActiveTools: (value: string[]) => { tools = value; },
  } as unknown as ExtensionAPI);
  const start = () => hooks.get("session_start")!({}, ctx);
  const run = async (text: string, signal?: AbortSignal) => {
    const events: AssistantMessageEvent[] = [];
    const stream = provider.streamSimple!(model, normalizeContext({messages:[{role:"user",content:text,timestamp:Date.now()}]}), {signal});
    for await (const event of stream) events.push(event);
    return {events, result: await stream.result()};
  };
  return {start, run, entries, hooks, ctx, registered, messages, notices, prompts, terminal,
    command: (args:string) => registered.get("tns")!.handler(args,ctx), setChoice: (value: string | undefined) => {choice=value;},
    tools: () => tools, close: () => hooks.get("session_shutdown")!()};
}

test("duplicate TNS extensions register once without disabling other extensions", async () => {
  const loader = new DefaultResourceLoader({
    cwd: dir, agentDir: join(dir, "duplicate-home"), noExtensions: true,
    noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true,
    extensionFactories: [extension, extension, pi => pi.registerCommand("unrelated", {
      description: "Unrelated extension", handler: async () => {},
    })],
  });
  for (let attempt = 0; attempt < 2; attempt++) {
    await loader.reload();
    const result = loader.getExtensions();
    assert.deepEqual(result.errors, []);
    const names = result.extensions.flatMap(e => [...e.commands.keys()]);
    assert.deepEqual(names.sort(), ["tns", "unrelated"]);
    assert.equal(result.runtime.pendingProviderRegistrations.filter(p => p.name === "tns").length, 1);
    // Pi invalidates the old runtime before reloading extension resources.
    result.runtime.invalidate();
  }
});

test("validates connections and preserves argument boundaries", () => {
  assert.throws(() => connection({kind:"codex",host:"-oProxyCommand=bad"}));
  assert.throws(() => connection({kind:"other",host:"host"}));
  assert.deepEqual(bridgeArgs(connection({kind:"claude",host:"host",cwd:"/path with spaces",extra:["--model","literal;value"]})),
    ["agent","--bridge","claude","host","--cwd","/path with spaces","--","--model","literal;value"]);
});

for (const kind of ["claude", "codex"] as const) {
  test(`${kind}: namespaced controls preserve Pi commands and remote sessions`, {timeout:15000}, async () => {
    const h = harness(kind);
    try {
      await h.start();
      assert.deepEqual([...h.registered.keys()],["tns"]);
      await h.command("help");
      assert.match(h.messages.at(-1)!, /Pi's own \/model/);
      await h.command("models");
      assert.match(h.messages.at(-1)!, /model-a/);
      if(kind === "codex") assert.match(h.messages.at(-1)!, /model-b/);
      h.setChoice("model-a — Remote model");
      await h.command("model");
      assert.equal((h.entries.at(-1)!.data as Connection).model,"model-a");
      await h.run("with selected model");
      const session=(h.entries.at(-1)!.data as Connection).resume;
      await h.command("model model-b");
      assert.equal((h.entries.at(-1)!.data as Connection).resume,session);
      await h.run("second model");
      if(kind === "claude") {
        await h.command("model bad");
        assert.match(h.notices.at(-1)!,/unknown model/);
        assert.equal((h.entries.at(-1)!.data as Connection).model,"model-b");
      }
      await h.command("compact");
      assert.deepEqual(h.prompts.at(-1),{content:"/compact",options:{expandPromptTemplates:false}});
      assert.equal((await h.run(h.prompts.at(-1)!.content)).result.stopReason,"stop");
      await h.command("commands");
      if(kind === "claude") {
        assert.match(h.messages.at(-1)!,/\/tns run \/custom-skill/);
        await h.command("run /context");
        const result=await h.run(h.prompts.at(-1)!.content);
        assert.match(JSON.stringify(result.result.content),/context from remote/);
        const count=h.prompts.length;
        await h.command("run /not-a-command");
        assert.equal(h.prompts.length,count);
        assert.match(h.notices.at(-1)!,/not advertised/);
      } else {
        assert.match(h.messages.at(-1)!,/native slash commands/);
        await h.command("run /review");
        assert.match(h.notices.at(-1)!,/belong to its TUI/);
      }
      h.close();
      await h.start();
      await h.run("after reconnect with model");
      const records=readFileSync(log,"utf8").trim().split("\n").map(line=>JSON.parse(line));
      if(kind === "codex") {
        assert.equal(records.filter(v=>v.method==="turn/start").at(-1).params.model,"model-b");
        assert.ok(records.some(v=>v.method==="thread/compact/start"));
      } else assert.ok(records.some(v=>v.request?.subtype==="set_model"&&v.request.model==="model-b"));
    } finally {h.close();}
  });

  test(`${kind}: native UI handoff resumes the remote session and restores Pi`, {timeout:15000}, async () => {
    const h=harness(kind);
    try {
      await h.start();
      await h.run("before native");
      await h.command("native");
      assert.deepEqual(h.terminal,["stop","start"]);
      assert.match(h.messages.at(-1)!,/Native .* exited \(0\)/);
      const launches=readFileSync(log,"utf8").trim().split("\n").map(line=>JSON.parse(line)).filter(v=>v.argv);
      assert.deepEqual(launches.at(-1).argv,kind === "claude" ? ["--resume","claude-saved"] : ["resume","codex-saved"]);
      assert.equal((await h.run("back in Pi")).result.stopReason,"stop");
    } finally {h.close();}
  });

  test(`${kind}: native streams, tools, permissions, persistence and resume`, {timeout:15000}, async () => {
    const h = harness(kind);
    try {
      await h.start();
      assert.deepEqual(h.tools(), []);
      const first = await h.run("hello 世界");
      assert.equal(first.result.stopReason, "stop");
      assert.match(JSON.stringify(first.result.content), /hello 世界/);
      assert.equal(first.events.filter(e => e.type === "done").length, 1);
      assert.equal(first.events.filter(e => e.type === "text_start").length, 1);
      const tool = await h.run("tool");
      assert.match(JSON.stringify(tool.result.content), /echo safe/);
      assert.equal(tool.events.some(e => e.type === "toolcall_start"), false);
      await h.run("permission");
      h.setChoice(undefined);
      await h.run("permission");
      const lines = readFileSync(log,"utf8");
      assert.match(lines, kind === "claude" ? /"behavior":"allow"/ : /"decision":"accept"/);
      assert.match(lines, kind === "claude" ? /"behavior":"deny"/ : /"decision":"decline"/);
      assert.equal((h.entries.at(-1)!.data as {resume:string}).resume, `${kind}-saved`);
      h.close();
      await h.start();
      assert.equal((await h.run("resumed")).result.stopReason,"stop");
      assert.match(readFileSync(log,"utf8"), kind === "claude" ? /"--resume","claude-saved"/ : /"thread\/resume"/);
      assert.deepEqual(h.hooks.get("session_before_fork")!({},h.ctx),{cancel:true});
      await h.hooks.get("session_before_switch")!({reason:"new"}, h.ctx);
      h.entries.length = 0;
      await h.start();
      assert.equal((h.entries.at(-1)!.data as {resume?:string}).resume, undefined);
    } finally { h.close(); }
  });

  test(`${kind}: cancellation and disconnect terminate streams`, {timeout:15000}, async () => {
    const h = harness(kind);
    try {
      await h.start();
      const signal = AbortSignal.timeout(250);
      const stopped = await h.run("hang", signal);
      assert.equal(stopped.result.stopReason,"aborted");
      assert.equal(stopped.events.filter(e => e.type === "error").length,1);
      const dropped = await h.run("drop");
      assert.equal(dropped.result.stopReason,"error");
      assert.equal((await h.run("after reconnect")).result.stopReason,"stop");
    } finally { h.close(); }
  });
}

test("native SSH handoff preserves argument boundaries", () => {
  const launch=nativeLaunch({kind:"claude",host:"server",cwd:"/path with ' and $stuff",resume:"id;$(false)",model:"model-a"});
  assert.deepEqual(launch.args.slice(0,-1),["-t","-o","BatchMode=yes","--","server"]);
  assert.match(launch.args.at(-1)!, /'id;\$\(false\)'/);
  assert.ok(launch.args.at(-1)!.includes("'\"'\"'"));
});

test("control timeouts reject all pending requests and close the process", {timeout:3000}, async () => {
  const binary=join(dir,"stalled-bridge");
  writeFileSync(binary,"#!/usr/bin/env node\nprocess.stdin.resume();\n",{mode:0o755});
  const bridge=new Bridge({kind:"claude",host:"",local:true},binary);
  const results=await Promise.allSettled([bridge.request("models",null,100),bridge.request("commands",null,1000)]);
  assert.equal(results[0].status,"rejected");
  assert.equal(results[1].status,"rejected");
  await bridge.close();
});

test("SSH transport quotes directory and agent arguments", {timeout:15000}, async () => {
  const quoted = join(dir,"space ' and $dollar");
  const {mkdirSync} = await import("node:fs");
  mkdirSync(quoted);
  const h=harness("codex",true,{cwd:quoted,extra:["--literal","literal;$(no-execute)"]});
  try {
    await h.start();
    assert.equal((await h.run("ssh")).result.stopReason,"stop");
    const launches=readFileSync(log,"utf8").trim().split("\n").map(line=>JSON.parse(line)).filter(v=>v.argv);
    assert.deepEqual(launches.at(-1).argv,["app-server","--literal","literal;$(no-execute)"]);
    assert.equal(launches.at(-1).cwd,realpathSync(quoted));
  }
  finally {h.close();}
});

test("tns agent launches Pi with the selected endpoint and binary", async () => {
  const {mkdirSync}=await import("node:fs");
  const launchDir=join(dir,"launcher");
  mkdirSync(launchDir);
  writeFileSync(join(launchDir,"pi"),`#!/usr/bin/env node\nconsole.log(JSON.stringify({argv:process.argv.slice(2),config:JSON.parse(process.env.TNS_AGENT_CONFIG),binary:process.env.TNS_BIN}));\n`,{mode:0o755});
  const result=spawnSync(process.env.TNS_BIN!,["agent","codex","host","--cwd","/remote dir","--resume","saved"],
    {encoding:"utf8",env:{...process.env,PATH:`${launchDir}:${process.env.PATH}`}});
  assert.equal(result.status,0,result.stderr);
  const launched=JSON.parse(result.stdout);
  assert.deepEqual(launched.config,{kind:"codex",host:"host",local:false,cwd:"/remote dir",resume:"saved",extra:[]});
  assert.equal(launched.binary,process.env.TNS_BIN);
  assert.deepEqual(launched.argv.slice(-5),["--provider","tns","--model","remote","--no-tools"]);
});

test("real Pi CLI loads the extension and uses the bridge without model credentials", {timeout:20000}, () => {
  process.env.TNS_AGENT_CONFIG=JSON.stringify({kind:"claude",host:"",local:true,cwd:dir});
  const result=spawnSync("pi",["-e",resolve("extensions/pi/index.ts"),"--provider","tns","--model","remote","--no-tools","--no-session","-p","pi smoke"],
    {encoding:"utf8",timeout:15000,env:{...process.env,PI_CODING_AGENT_DIR:join(dir,"pi-home")}});
  assert.equal(result.status,0,result.stderr);
  assert.match(result.stdout,/reply: pi smoke/);
});

test("real Pi routes /tns model and compact to the remote adapter", {timeout:15000}, async () => {
  const child=spawn("pi",["-e",resolve("extensions/pi/index.ts"),"--mode","rpc","--provider","tns","--model","remote","--no-tools","--no-session"],
    {env:{...process.env,PI_CODING_AGENT_DIR:join(dir,"rpc-home"),TNS_AGENT_CONFIG:JSON.stringify({kind:"codex",host:"",local:true,cwd:dir})}});
  const lines=createInterface({input:child.stdout});
  const events: Record<string,unknown>[]=[];
  let stderr="";
  child.stderr.on("data",data=>{stderr+=data;});
  const waiters=new Set<()=>void>();
  lines.on("line",line=>{events.push(JSON.parse(line)); for(const notify of waiters)notify();});
  const waitFor=(predicate:(event:Record<string,unknown>)=>boolean) => new Promise<void>((resolve,reject)=>{
    const timer=setTimeout(()=>{waiters.delete(check);reject(new Error(`Pi RPC timeout: ${stderr}`));},10000);
    const check=()=>{if(events.some(predicate)){clearTimeout(timer);waiters.delete(check);resolve();}};
    waiters.add(check);check();
  });
  try {
    child.stdin.write(JSON.stringify({id:"choose",type:"prompt",message:"/tns model model-b"})+"\n");
    await waitFor(e=>e.type==="response"&&e.id==="choose");
    child.stdin.write(JSON.stringify({id:"compact",type:"prompt",message:"/tns compact"})+"\n");
    await waitFor(e=>e.type==="agent_end");
    const records=readFileSync(log,"utf8").trim().split("\n").map(line=>JSON.parse(line));
    assert.ok(records.some(v=>v.method==="thread/compact/start"));
    assert.ok(!events.some(e=>e.type==="extension_error"),JSON.stringify(events));
  } finally {
    lines.close();child.stdin.end();child.kill();
    await new Promise(resolve=>child.once("close",resolve));
  }
});
