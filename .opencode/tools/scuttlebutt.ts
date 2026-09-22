import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { tool } from "@opencode-ai/plugin";

const execFileAsync = promisify(execFile);

function executable(): string {
  return process.env.SCUTTLEBUTT_BIN || "scuttlebutt";
}

function maybeGroup(group?: string): string[] {
  return group ? ["--group", group] : [];
}

async function runScuttlebutt(args: string[], context: any): Promise<string> {
  const { stdout, stderr } = await execFileAsync(executable(), args, {
    cwd: context.directory || context.worktree || process.cwd(),
    env: process.env,
    timeout: 15_000,
    maxBuffer: 1024 * 1024,
  });
  return [stdout, stderr].filter(Boolean).join("\n").trim() || "ok";
}

export const post = tool({
  description: "Post a short message to the herdr session's shared Scuttlebutt room. Use for status, blockers, handoff notes, or information useful to other agents. Keep it under 80 words.",
  args: {
    text: tool.schema.string().describe("Message to post. Keep it under 80 words."),
    group: tool.schema.string().optional().describe("Optional room/group override."),
  },
  async execute(args, context) {
    return runScuttlebutt(["post", args.text, ...maybeGroup(args.group)], context);
  },
});

export const read = tool({
  description: "Read recent messages from the herdr session's shared Scuttlebutt room.",
  args: {
    since: tool.schema.number().optional().describe("Only messages with id greater than this."),
    limit: tool.schema.number().optional().describe("Max messages when since is omitted."),
    group: tool.schema.string().optional().describe("Optional room/group override."),
  },
  async execute(args, context) {
    const cmd = ["read", ...maybeGroup(args.group)];
    if (args.since !== undefined) cmd.push("--since", String(args.since));
    if (args.limit !== undefined) cmd.push("--limit", String(args.limit));
    return runScuttlebutt(cmd, context);
  },
});

export const agents = tool({
  description: "List agents in the current Scuttlebutt room.",
  args: {
    group: tool.schema.string().optional().describe("Optional room/group override."),
  },
  async execute(args, context) {
    return runScuttlebutt(["agents", ...maybeGroup(args.group)], context);
  },
});
