import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const execFileAsync = promisify(execFile);

const HELP = `Scuttlebutt is the shared chat room for this herdr session.
- Use scuttlebutt_post when you have status, blockers, handoff notes, or information useful to other agents.
- Use scuttlebutt_read to catch up if you need recent room context.
- Keep posts under 80 words; put long detail on the issue/PR and post a short summary.
- Do not acknowledge or repeat messages unless you have new information.`;

function enabled(): boolean {
  return Boolean(process.env.HERDR_SOCKET_PATH || process.env.HERDR_PANE_ID || process.env.SCUTTLEBUTT_DIR);
}

function executable(): string {
  return process.env.SCUTTLEBUTT_BIN || "scuttlebutt";
}

async function runScuttlebutt(args: string[], ctx: ExtensionContext): Promise<string> {
  const { stdout, stderr } = await execFileAsync(executable(), args, {
    cwd: ctx.cwd,
    env: process.env,
    timeout: 15_000,
    maxBuffer: 1024 * 1024,
  });
  return [stdout, stderr].filter(Boolean).join("\n").trim() || "ok";
}

function maybeGroup(group?: string): string[] {
  return group ? ["--group", group] : [];
}

export default function (pi: ExtensionAPI) {
  pi.on("before_agent_start", async (event) => {
    if (!enabled()) return;
    if (event.systemPrompt.includes("Scuttlebutt is the shared chat room for this herdr session")) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${HELP}` };
  });

  pi.registerTool({
    name: "scuttlebutt_post",
    label: "Scuttlebutt post",
    description: "Post a short message to the herdr session's shared Scuttlebutt room.",
    parameters: Type.Object({
      text: Type.String({ description: "Message to post. Keep it under 80 words." }),
      group: Type.Optional(Type.String({ description: "Optional room/group override." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      const out = await runScuttlebutt(["post", params.text, ...maybeGroup(params.group)], ctx);
      return { content: [{ type: "text", text: out }], details: {} };
    },
  });

  pi.registerTool({
    name: "scuttlebutt_read",
    label: "Scuttlebutt read",
    description: "Read recent messages from the herdr session's shared Scuttlebutt room.",
    parameters: Type.Object({
      since: Type.Optional(Type.Number({ description: "Only messages with id greater than this." })),
      limit: Type.Optional(Type.Number({ description: "Max messages when since is omitted." })),
      group: Type.Optional(Type.String({ description: "Optional room/group override." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      const args = ["read", ...maybeGroup(params.group)];
      if (params.since !== undefined) args.push("--since", String(params.since));
      if (params.limit !== undefined) args.push("--limit", String(params.limit));
      const out = await runScuttlebutt(args, ctx);
      return { content: [{ type: "text", text: out }], details: {} };
    },
  });

  pi.registerTool({
    name: "scuttlebutt_agents",
    label: "Scuttlebutt agents",
    description: "List agents in the current Scuttlebutt room.",
    parameters: Type.Object({
      group: Type.Optional(Type.String({ description: "Optional room/group override." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      const out = await runScuttlebutt(["agents", ...maybeGroup(params.group)], ctx);
      return { content: [{ type: "text", text: out }], details: {} };
    },
  });
}
