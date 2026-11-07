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
let stopping = false; // set when OMP itself exits; until then a dead channel is restarted
let restartDelay = 1000;
let exitHooked = false;
const waiting = new Map<number, Pending>();

/** The agent's channel profile, worked out as warren's `matrix::profile`
 *  does: the channel takes only lowercase names. */
function profile(agent: string): string {
	const p = agent.toLowerCase().replace(/[^a-z0-9._-]/g, "-");
	return /^[a-z0-9]/.test(p) ? p : `w${p}`;
}

function readServers(): Record<string, Spec> {
	const agent = process.env.WARREN_AGENT;
	if (!agent) return {};
	const home = process.env.WARREN_HOME || join(homedir(), ".warren");
	try {
		const mcp = JSON.parse(readFileSync(join(home, "matrix", profile(agent), "mcp.json"), "utf8"));
		return mcp?.mcpServers ?? {};
	} catch {
		return {};
	}
}

function readSpec(): Spec | undefined {
	return readServers()[SERVER];
}

// Sidecars (talkback, marginalia, …): the other servers in the same file.
// Only what they push comes through here; their tools stay on their HTTP
// APIs, which an OMP agent can call directly.
let sidecarsStarted = false;
const sidecarProcs = new Set<ChildProcess>();
const sidecarNotes = new Map<string, string>();

function startSidecar(name: string, spec: Spec, deliver: (source: string, params: any) => void) {
	let delay = 1000;
	const launch = () => {
		if (stopping) return;
		const started = Date.now();
		const proc = spawn(spec.command, spec.args ?? [], {
			env: { ...process.env, ...(spec.env ?? {}) },
			stdio: ["pipe", "pipe", "ignore"],
		});
		sidecarProcs.add(proc);
		const write = (m: object) => proc.stdin?.write(`${JSON.stringify(m)}\n`);
		let buf = "";
		proc.stdout!.setEncoding("utf8");
		proc.stdout!.on("data", (chunk: string) => {
			buf += chunk;
			for (let i = buf.indexOf("\n"); i >= 0; i = buf.indexOf("\n")) {
				const line = buf.slice(0, i).trim();
				buf = buf.slice(i + 1);
				if (!line) continue;
				let msg: any;
				try {
					msg = JSON.parse(line);
				} catch {
					continue;
				}
				if (msg.id === 1 && !msg.method) {
					if (msg.result?.instructions) sidecarNotes.set(name, msg.result.instructions);
					write({ jsonrpc: "2.0", method: "notifications/initialized" });
				} else if (msg.method === "notifications/claude/channel") {
					deliver(name, msg.params);
				} else if (msg.id != null && msg.method) {
					write({ jsonrpc: "2.0", id: msg.id, result: {} });
				}
			}
		});
		proc.on("exit", () => {
			sidecarProcs.delete(proc);
			if (stopping) return;
			delay = Date.now() - started > 60_000 ? 1000 : Math.min(delay * 2, 60_000);
			setTimeout(launch, delay).unref?.();
		});
		write({
			jsonrpc: "2.0",
			id: 1,
			method: "initialize",
			params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "warren-omp", version: "1" } },
		});
	};
	launch();
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
function asChannelEvent(params: any, source: string = SERVER): string {
	const meta: Record<string, unknown> = params?.meta ?? {};
	const esc = (v: unknown) => String(v).replaceAll("&", "&amp;").replaceAll('"', "&quot;");
	const attrs = Object.entries(meta)
		.map(([k, v]) => ` ${k}="${esc(v)}"`)
		.join("");
	return `<channel source="${source}"${attrs}>${params?.content ?? ""}</channel>`;
}

/** A freeform matrix_reply input: "ROOM [REPLY_TO]\nTEXT", the text untouched. */
export function replyFromInput(input: string): { room: string; text: string; reply_to?: string } {
	const nl = input.indexOf("\n");
	const head = (nl < 0 ? input : input.slice(0, nl)).trim();
	const text = nl < 0 ? "" : input.slice(nl + 1);
	const [room, replyTo] = head.split(/\s+/);
	return replyTo ? { room, text, reply_to: replyTo } : { room, text };
}

export default function warrenMatrix(pi: ExtensionAPI) {
	const spec = readSpec();
	if (!spec) return;
	const z = pi.zod;

	function deliver(params: any, source: string = SERVER) {
		// "aside": between steps of whatever the agent is doing, without cutting
		// a tool batch short; a turn of its own when it is idle.
		pi.sendMessage(
			{
				customType: source === SERVER ? "matrix" : source,
				content: asChannelEvent(params, source),
				display: true,
				attribution: "agent",
			},
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
		const started = Date.now();
		proc.on("exit", () => {
			if (child === proc) {
				child = undefined;
				ready = undefined;
				// Messages only arrive while it runs, so bring it back rather than
				// waiting for the next tool call; back off if it keeps dying.
				if (!stopping) {
					restartDelay = Date.now() - started > 60_000 ? 1000 : Math.min(restartDelay * 2, 60_000);
					setTimeout(() => {
						if (!stopping && !child) start().catch(() => {});
					}, restartDelay).unref?.();
				}
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
		if (!sidecarsStarted) {
			sidecarsStarted = true;
			for (const [name, sc] of Object.entries(readServers())) {
				if (name !== SERVER && sc?.command) startSidecar(name, sc, (src, p) => deliver(p, src));
			}
		}
		await start().catch(() => {});
	});
	// Not on session_shutdown: subagent sessions end inside the main one and
	// share this channel. It stops with OMP (and on its own when its stdin closes).
	if (!exitHooked) {
		exitHooked = true;
		process.on("exit", () => {
			stopping = true;
			child?.kill();
			for (const p of sidecarProcs) p.kill();
		});
	}

	// The channel's instructions, as Claude Code would have put them in front
	// of the model, plus the one difference: the tools' names here.
	pi.on("before_agent_start", async () => {
		if (!instructions && sidecarNotes.size === 0) return;
		let append = instructions
			? `\n# Matrix chat\n${instructions}\n` +
				"In this session the chat tools are named matrix_claim_name, matrix_reply, matrix_react, " +
				"matrix_send_file, matrix_fetch_messages, matrix_download, matrix_set_avatar and matrix_search.\n" +
				"Send messages by calling matrix_reply directly, not from eval code: its input is plain text (the " +
				"room on the first line, the message after it), so TeX is written as is, with single backslashes.\n"
			: "";
		for (const [name, note] of sidecarNotes) {
			append +=
				`\n# ${name}\n${note}\n` +
				`In this session ${name}'s events arrive as <channel source="${name}"> messages, but its tools are ` +
				"not loaded: do what they would do through its HTTP API.\n";
		}
		return { systemPromptAppend: append };
	});

	// The channel's tools, declared here: OMP wants its own schemas, and
	// these are stable. Before the agent claims a name, every one but
	// claim_name answers with the channel's own "claim a name first".
	const tool = (
		name: string,
		description: string,
		parameters: any,
		{ fromInput, ...extra }: { fromInput?: (input: string) => any; [k: string]: any } = {},
	) =>
		pi.registerTool({
			name: `matrix_${name}`,
			label: `Matrix ${name.replaceAll("_", " ")}`,
			description,
			parameters,
			...extra,
			async execute(_id: string, params: any) {
				await start();
				// A freeform call arrives as { input: "<the raw text>" }.
				if (fromInput && typeof params?.input === "string") params = fromInput(params.input);
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
	// reply is a top-level tool, not one reached from eval, and on models that
	// take freeform tools (GPT on the Responses API) its input is plain text:
	// the room line, then the message as written. Nothing between the model and
	// the channel then reads escapes, so TeX like \rho or \frac arrives intact.
	// A message written as a JSON string, or as a string in eval code, has had
	// its backslashes read as escapes (\r a carriage return, \\ one backslash).
	tool(
		"reply",
		"Send a text message (Markdown allowed) to a Matrix room. As freeform input: the first line is the " +
			"room (id !..., alias #...:server, or name), optionally followed by a space and the event_id to reply " +
			"to; every later line is the message, sent exactly as written. Write TeX as you would in a .tex file, " +
			"with single backslashes: nothing is unescaped.",
		// OMP checks a freeform call's { input } against this schema too, and drops
		// keys it doesn't list, so input is here and the rest are optional; the
		// channel itself refuses a reply without a room or text.
		z.object({
			input: z.string().describe("Freeform form: the room line, then the message").optional(),
			room: z.string().describe("Room id (!...) or alias (#name:server)").optional(),
			text: z.string().optional(),
			reply_to: z.string().describe("event_id to reply to (optional)").optional(),
		}),
		{
			loadMode: "essential",
			customWireName: "matrix_reply",
			customFormat: {
				syntax: "lark",
				definition: [
					"start: ROOM REPLY_TO? NEWLINE TEXT",
					"ROOM: /[^\\s$][^\\s]*/",
					"REPLY_TO: / +\\$[^\\s]+/",
					"NEWLINE: /\\r?\\n/",
					"TEXT: /[\\s\\S]+/",
				].join("\n"),
			},
			fromInput: replyFromInput,
		},
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
