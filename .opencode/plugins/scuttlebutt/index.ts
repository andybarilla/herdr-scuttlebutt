import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { Plugin } from "@opencode/plugin";

const execFileAsync = promisify(execFile);

const HELP = `Scuttlebutt is the shared chat room for this herdr session.
- Use the scuttlebutt_post tool when you have status, blockers, handoff notes, or information useful to other agents.
- Use the scuttlebutt_read tool to catch up if you need recent room context.
- Keep posts under 80 words; put long detail on the issue/PR and post a short summary.
- Do not acknowledge or repeat messages unless you have new information.`;

type ToolContext = { signal?: AbortSignal; progress?: (input: { status: string }) => Promise<void> | void };

type PostInput = { text: string; group?: string };
type ReadInput = { since?: number; limit?: number; group?: string };
type AgentsInput = { group?: string };

function enabled(): boolean {
  return Boolean(process.env.HERDR_SOCKET_PATH || process.env.HERDR_PANE_ID || process.env.SCUTTLEBUTT_DIR);
}

function executable(): string {
  return process.env.SCUTTLEBUTT_BIN || "scuttlebutt";
}

function cwd(ctx: any): string {
  return ctx.location?.project?.canonical || ctx.location?.project?.worktree || process.cwd();
}

function maybeGroup(group?: string): string[] {
  return group ? ["--group", group] : [];
}

async function runScuttlebutt(args: string[], ctx: any, toolCtx?: ToolContext): Promise<string> {
  await toolCtx?.progress?.({ status: `scuttlebutt ${args[0]}` });
  const { stdout, stderr } = await execFileAsync(executable(), args, {
    cwd: cwd(ctx),
    env: process.env,
    timeout: 15_000,
    maxBuffer: 1024 * 1024,
    signal: toolCtx?.signal,
  });
  return [stdout, stderr].filter(Boolean).join("\n").trim() || "ok";
}

export default Plugin.define({
  id: "scuttlebutt",
  async setup(ctx) {
    if (enabled()) {
      await ctx.session.hook("context", (event) => {
        event.system.push({ type: "text", text: HELP });
      });
    }

    await ctx.tool.transform((editor) => {
      editor.namespace({
        name: "scuttlebutt",
        description: "Shared herdr session chat room for coordinating agents.",
      });

      editor.add({
        name: "post",
        description: "Post a short message to the herdr session's shared Scuttlebutt room.",
        input: {
          type: "object",
          properties: {
            text: { type: "string", description: "Message to post. Keep it under 80 words." },
            group: { type: "string", description: "Optional room/group override." },
          },
          required: ["text"],
          additionalProperties: false,
        },
        options: { namespace: "scuttlebutt", codemode: true },
        execute: async (input, toolCtx) => ({
          content: await runScuttlebutt(["post", (input as PostInput).text, ...maybeGroup((input as PostInput).group)], ctx, toolCtx),
        }),
      });

      editor.add({
        name: "read",
        description: "Read recent messages from the herdr session's shared Scuttlebutt room.",
        input: {
          type: "object",
          properties: {
            since: { type: "number", description: "Only messages with id greater than this." },
            limit: { type: "number", description: "Max messages when since is omitted." },
            group: { type: "string", description: "Optional room/group override." },
          },
          additionalProperties: false,
        },
        options: { namespace: "scuttlebutt", codemode: true },
        execute: async (input, toolCtx) => {
          const params = input as ReadInput;
          const args = ["read", ...maybeGroup(params.group)];
          if (params.since !== undefined) args.push("--since", String(params.since));
          if (params.limit !== undefined) args.push("--limit", String(params.limit));
          return { content: await runScuttlebutt(args, ctx, toolCtx) };
        },
      });

      editor.add({
        name: "agents",
        description: "List agents in the current Scuttlebutt room.",
        input: {
          type: "object",
          properties: {
            group: { type: "string", description: "Optional room/group override." },
          },
          additionalProperties: false,
        },
        options: { namespace: "scuttlebutt", codemode: true },
        execute: async (input, toolCtx) => ({
          content: await runScuttlebutt(["agents", ...maybeGroup((input as AgentsInput).group)], ctx, toolCtx),
        }),
      });
    });
  },
});
