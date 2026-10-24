/**
 * warren's Matrix chat for OMP agents.
 *
 * Claude Code takes the matrix channel as an MCP "channel": chat messages are
 * pushed into the running session. OMP has no channels, so this extension does
 * that job itself. It runs the agent's own channel server (the one warren set
 * up in ~/.warren/matrix/<agent>/mcp.json), offers its tools, and turns each
 * pushed message into a message in the session, the way a channel would.
 *
 * warren loads it with `omp -e` for agents that have chat on, and writes this
 * file from the copy built into warren, so the two never drift apart.
 */
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { type ChildProcess, spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const SERVER = "warren-matrix";

type Spec = { command: string; args?: string[]; env?: Record<string, string> };
type Pending = { resolve: (v: any) => void; reject: (e: Error) => void };

// One channel per OMP process. Subagent sessions load extensions again; they
// share this one rather than each starting a second copy on the same account,
// which would deliver every message twice.
let child: ChildProcess | undefined;
let ready: Promise<void> | undefined;
let instructions = "";
let nextId = 1;
const waiting = new Map<number, Pending>();

/** The agent's channel profile, worked out as warren's `matrix::profile`
 *  does: the channel takes only lowercase names. */
function profile(agent: string): string {
	const p = agent.toLowerCase().replace(/[^a-z0-9._-]/g, "-");
	return /^[a-z0-9]/.test(p) ? p : `w${p}`;
}

function readSpec(): Spec | undefined {
	const agent = process.env.WARREN_AGENT;
	if (!agent) return undefined;
	const home = process.env.WARREN_HOME || join(homedir(), ".warren");
	try {
		const mcp = JSON.parse(readFileSync(join(home, "matrix", profile(agent), "mcp.json"), "utf8"));
		return mcp?.mcpServers?.[SERVER];
	} catch {
		return undefined;
	}
}

function send(msg: object) {
	child?.stdin?.write(`${JSON.stringify(msg)}\n`);
}

function rpc(method: string, params: object): Promise<any> {
	const id = nextId++;
	return new Promise((resolve, reject) => {
		waiting.set(id, { resolve, reject });
		send({ jsonrpc: "2.0", id, method, params });
	});
}

/** A chat message as Claude Code shows a channel event, so the channel's own
 *  instructions read the same in both harnesses. */
function asChannelEvent(params: any): string {
	const meta: Record<string, unknown> = params?.meta ?? {};
	const esc = (v: unknown) => String(v).replaceAll("&", "&amp;").replaceAll('"', "&quot;");
	const attrs = Object.entries(meta)
		.map(([k, v]) => ` ${k}="${esc(v)}"`)
		.join("");
	return `<channel source="${SERVER}"${attrs}>${params?.content ?? ""}</channel>`;
}

export default function warrenMatrix(pi: ExtensionAPI) {
	const spec = readSpec();
	if (!spec) return;
	const z = pi.zod;

	function deliver(params: any) {
		// "aside": between steps of whatever the agent is doing, without cutting
		// a tool batch short; a turn of its own when it is idle.
		pi.sendMessage(
			{ customType: "matrix", content: asChannelEvent(params), display: true, attribution: "agent" },
			{ deliverAs: "aside", triggerTurn: true },
		);
	}

	function onLine(line: string) {
		let msg: any;
		try {
			msg = JSON.parse(line);
		} catch {
			return;
		}
		if (msg.id != null && !msg.method) {
			const w = waiting.get(msg.id);
			if (!w) return;
			waiting.delete(msg.id);
			if (msg.error) w.reject(new Error(msg.error.message ?? "the chat channel refused"));
			else w.resolve(msg.result);
			return;
		}
		if (msg.method === "notifications/claude/channel") deliver(msg.params);
		// A request from the server (ping): answer it, with nothing.
		if (msg.id != null && msg.method) send({ jsonrpc: "2.0", id: msg.id, result: {} });
	}

	function start() {
		if (ready) return ready;
		const proc = spawn(spec!.command, spec!.args ?? [], {
			env: { ...process.env, ...(spec!.env ?? {}) },
			stdio: ["pipe", "pipe", "ignore"], // its stderr goes to the log file its command names
		});
		child = proc;
		let buf = "";
		proc.stdout!.setEncoding("utf8");
		proc.stdout!.on("data", (chunk: string) => {
			buf += chunk;
			for (let i = buf.indexOf("\n"); i >= 0; i = buf.indexOf("\n")) {
				const line = buf.slice(0, i);
				buf = buf.slice(i + 1);
				if (line.trim()) onLine(line);
			}
		});
		proc.on("exit", () => {
			if (child === proc) {
				child = undefined;
				ready = undefined; // the next tool call starts it again
			}
			for (const w of waiting.values()) w.reject(new Error("the chat channel stopped"));
			waiting.clear();
		});
		ready = (async () => {
			const init = await rpc("initialize", {
				protocolVersion: "2025-06-18",
				capabilities: {},
				clientInfo: { name: "warren-omp", version: "1" },
			});
			instructions = init?.instructions ?? "";
			send({ jsonrpc: "2.0", method: "notifications/initialized" });
		})();
		return ready;
	}

	pi.on("session_start", async () => {
		await start().catch(() => {});
	});
	pi.on("session_shutdown", () => {
		child?.kill();
	});

	// The channel's instructions, as Claude Code would have put them in front
	// of the model, plus the one difference: the tools' names here.
	pi.on("before_agent_start", async () => {
		if (!instructions) return;
		return {
			systemPromptAppend:
				`\n# Matrix chat\n${instructions}\n` +
				"In this session the chat tools are named matrix_claim_name, matrix_reply, matrix_react, " +
				"matrix_send_file, matrix_fetch_messages, matrix_download, matrix_set_avatar and matrix_search.\n",
		};
	});

	// The channel's tools, declared here: OMP wants its own schemas, and
	// these are stable. Before the agent claims a name, every one but
	// claim_name answers with the channel's own "claim a name first".
	const tool = (name: string, description: string, parameters: any) =>
		pi.registerTool({
			name: `matrix_${name}`,
			label: `Matrix ${name.replaceAll("_", " ")}`,
			description,
			parameters,
			async execute(_id: string, params: any) {
				await start();
				const res = await rpc("tools/call", { name, arguments: params ?? {} });
				const content = res?.content ?? [{ type: "text", text: JSON.stringify(res) }];
				if (res?.isError) {
					throw new Error(content.map((c: any) => c.text ?? "").join("\n") || "the chat channel refused");
				}
				return { content, details: {} };
			},
		});

	tool(
		"claim_name",
		"Pick your chat name. The first call registers the Matrix account @<name>.<machine> (permanent; if the " +
			"name is taken you get an error and should pick another) and connects. Later calls only change your " +
			"display name.",
		z.object({ name: z.string().describe("2-24 characters: lowercase letters, digits, - or _") }),
	);
	tool(
		"reply",
		"Send a text message (Markdown allowed) to a Matrix room.",
		z.object({
			room: z.string().describe("Room id (!...) or alias (#name:server)"),
			text: z.string(),
			reply_to: z.string().describe("event_id to reply to (optional)").optional(),
		}),
	);
	tool(
		"react",
		"React to a message with an emoji.",
		z.object({ room: z.string(), event_id: z.string(), emoji: z.string() }),
	);
	tool(
		"send_file",
		"Upload a local file and post it to a room (m.image for images, else m.file). The optional caption is " +
			"sent as a separate text message.",
		z.object({ room: z.string(), path: z.string(), caption: z.string().optional() }),
	);
	tool(
		"fetch_messages",
		"Messages in a room, oldest first. Without since: the latest ones, to catch up on context. With " +
			'since=<event_id>: the ones after that event, ending with a cursor to page on. With since="last": ' +
			"everything after your read marker for that room, and the marker moves forward, so repeated calls " +
			"never skip a message.",
		z.object({
			room: z.string(),
			limit: z.number().int().min(1).max(100).optional(),
			since: z.string().describe('An event_id, or "last" for your read marker').optional(),
			mentions_me: z.boolean().describe("Only messages that mention you").optional(),
			starts_with: z
				.string()
				.describe("Only messages whose text starts with this (case-insensitive; leading markdown is ignored)")
				.optional(),
		}),
	);
	tool(
		"download",
		"Download an mxc:// attachment. Saves under the channel's state dir (downloads/) unless dest_dir is " +
			"given; never overwrites. Returns the saved path.",
		z.object({ mxc: z.string(), dest_dir: z.string().optional() }),
	);
	tool(
		"set_avatar",
		"Set your Matrix profile picture, from a local image file (uploaded first) or an mxc:// URI that is " +
			"already uploaded. Pass exactly one. Returns the mxc:// URI.",
		z.object({
			path: z.string().describe("Local image file (png, jpeg, gif, webp, ...)").optional(),
			mxc: z.string().describe("An mxc://server/id URI").optional(),
		}),
	);
	tool(
		"search",
		"Full-text search of message history in the rooms this channel listens in (and the roster room), newest " +
			"first. Each hit has its event_id, so you can cite it or reply_to it.",
		z.object({
			query: z.string().describe("Words to search for"),
			room: z.string().describe("Only this room (id, alias or bare name); default: all of them").optional(),
			sender: z.string().describe("Only messages from this user id (@name:server)").optional(),
			since: z.string().describe("ISO date or datetime (UTC if no offset), inclusive").optional(),
			until: z.string().describe("ISO date or datetime; a bare date includes that whole day").optional(),
			limit: z.number().int().min(1).max(50).optional(),
		}),
	);
}
