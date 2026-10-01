// coucou-relay v1 — Coucou's omp hook. Forwards session events to the island
// over a Unix socket; nothing ever blocks the session.
//
// Wire: one JSON line per event carrying { "hook_event_name": SessionStart,
// PreToolUse, PermissionRequest, … } — the vocabulary the island front end
// already speaks, so no per-agent special casing exists anywhere.
//
// Budgets: 300 ms to connect. Coucou closed → the connect fails immediately
// (ENOENT/ECONNREFUSED) → one dropped event, omp carries on untouched. A relay
// that cannot send must be invisible to the session.
//
// Approval (the island's Allow/Deny):
//   * Asked only when omp itself would NOT have asked: tools.approvalMode is
//     yolo (the default) and the tool has no explicit tools.approval entry.
//     Any other mode — or an explicit per-tool policy — leaves the decision
//     to the terminal, exactly as configured, and this relay stays quiet
//     (asking in both places would be worse than asking in neither).
//   * Asked only in interactive sessions (ctx.hasUI): a print run must never
//     stall for a human.
//   * PermissionRequest keeps its connection open and waits for the island's
//     bare `allow` / `deny` word (Rust answers within 108 s).
//   * Only an explicit `deny` blocks the tool ({ block: true, reason }).
//     No answer — island paused, card never shown, Coucou closed, app died —
//     runs the tool: the absence of an answer never blocks the agent, and the
//     configured baseline (yolo) already allows it.
//
// Installed to ~/.omp/agent/hooks/post/coucou-relay.ts by the Coucou app.

import { connect } from "node:net";
import { tmpdir } from "node:os";

const CONNECT_BUDGET_MS = 300;
/** Longest string forwarded for any single field; the island truncates far less. */
const MAX_FIELD_LEN = 2_000;
/** Fields that are pointless to forward and can be enormous. */
const DROPPED_FIELDS = ["tool_response", "transcript_path", "content"];
/**
 * Tools the island asks about. Mutations, execution, delegation — the things
 * approval modes exist for. Read-only tools (read/grep/glob/ls/…) never ask.
 */
const ASK_TOOLS: Record<string, true> = {
  bash: true,
  write: true,
  edit: true,
  task: true,
  computer: true,
  browser: true,
};
/** Longest the island may take: Rust answers first at 108 s; this is the backstop. */
const PERMISSION_BUDGET_MS = 110_000;

type Loose = Record<string, any>;

function socketPath(): string {
  const runtime = process.env.XDG_RUNTIME_DIR;
  if (runtime && runtime.startsWith("/")) return `${runtime}/coucou.sock`;
  const uid = typeof process.getuid === "function" ? process.getuid() : 0;
  return `${tmpdir()}/coucou-${uid}.sock`;
}

/** Caps every string and drops the heavy fields — the island never shows them. */
function sanitize(value: unknown): unknown {
  if (typeof value === "string") {
    return value.length > MAX_FIELD_LEN ? `${value.slice(0, MAX_FIELD_LEN)}…` : value;
  }
  if (Array.isArray(value)) return value.map(sanitize);
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) {
      if (DROPPED_FIELDS.includes(k)) continue;
      out[k] = sanitize(v);
    }
    return out;
  }
  return value;
}

/** Fire-and-forget: connect (≤300 ms), write one line, close. Never throws. */
function send(payload: Record<string, unknown>): void {
  try {
    const line = JSON.stringify(sanitize(payload)) + "\n";
    const sock = connect(socketPath());
    let closed = false;
    const finish = () => {
      if (!closed) {
        closed = true;
        sock.destroy();
      }
    };
    const timer = setTimeout(finish, CONNECT_BUDGET_MS);
    sock.on("connect", () => {
      sock.write(line, () => {
        clearTimeout(timer);
        finish();
      });
    });
    sock.on("error", () => {
      clearTimeout(timer);
      finish();
    });
  } catch {
    // A relay that cannot send must be invisible to the session.
  }
}

/**
 * Ask the island for a permission decision. Keeps the connection open, waits
 * for the bare word Rust writes back.
 *
 * "deny" only on an explicit `deny`; everything else — `allow`, a closed
 * connection with no word (island declined, paused, or gave up), a timeout,
 * Coucou not running — resolves to null, which the caller reads as "run it".
 */
function askIsland(payload: Record<string, unknown>): Promise<"deny" | null> {
  const { promise, resolve } = Promise.withResolvers<"deny" | null>();
  let sock;
  try {
    sock = connect(socketPath());
  } catch {
    resolve(null);
    return promise;
  }
  let settled = false;
  let buffer = "";
  const finish = (value: "deny" | null) => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    sock.destroy();
    resolve(value);
  };
  const timer = setTimeout(() => finish(null), PERMISSION_BUDGET_MS);

  sock.on("connect", () => {
    sock.write(JSON.stringify(sanitize(payload)) + "\n");
  });
  sock.on("data", (chunk: Buffer) => {
    buffer += chunk.toString("utf8");
    const nl = buffer.indexOf("\n");
    if (nl < 0) return;
    const word = buffer.slice(0, nl).trim();
    if (word === "deny") finish("deny");
    else if (word === "allow") finish(null);
    // Anything else is not a decision — keep waiting for a real one.
  });
  sock.on("close", () => finish(null));
  sock.on("error", () => finish(null));
  return promise;
}

// Both extractors below are called from all event handlers and must stay
// identical for every one of them — lockstep over 3+ call sites, not a rename.
function cwdOf(ctx: Loose | undefined): string {
  return typeof ctx?.cwd === "string" ? ctx.cwd : "";
}

function sessionIdOf(ctx: Loose | undefined): string {
  try {
    return ctx?.sessionManager?.getSessionId?.() ?? "";
  } catch {
    return "";
  }
}

export default function hook(pi: Loose): void {
  pi.on("session_start", (_event: unknown, ctx: Loose) => {
    send({ hook_event_name: "SessionStart", cwd: cwdOf(ctx), session_id: sessionIdOf(ctx) });
  });

  pi.on("input", (event: Loose, ctx: Loose) => {
    send({
      hook_event_name: "UserPromptSubmit",
      cwd: cwdOf(ctx),
      session_id: sessionIdOf(ctx),
      prompt: typeof event?.text === "string" ? event.text : "",
    });
  });

  pi.on("tool_call", async (event: Loose, ctx: Loose) => {
    const toolName = typeof event?.toolName === "string" ? event.toolName : "tool";
    const base = {
      hook_event_name: "PreToolUse",
      cwd: cwdOf(ctx),
      session_id: sessionIdOf(ctx),
      tool_name: toolName,
      tool_input: event?.input && typeof event.input === "object" ? event.input : {},
    };
    send(base);

    // ── Approval: where omp would not have asked, the island asks. ──────────
    if (!ctx?.hasUI) return; // print/RPC: never stall a headless run
    if (!Object.hasOwn(ASK_TOOLS, toolName)) return;
    let mode: unknown;
    let policies: unknown;
    try {
      mode = pi?.pi?.settings?.get?.("tools.approvalMode");
      policies = pi?.pi?.settings?.get?.("tools.approval");
    } catch {
      // Settings unreachable → treat as default (yolo) and proceed to ask.
    }
    // Any mode other than yolo means the terminal's approval gate already
    // covers the tiers that prompt — asking twice would be worse than asking
    // once, so the relay stays quiet and the configured behavior wins.
    if (mode != null && mode !== "yolo") return;
    // An explicit per-tool policy is the user's own decision, honored in every
    // mode — including yolo. Their policy, their terminal, not our island.
    if (policies && typeof policies === "object" && (policies as Loose)[toolName] !== undefined) {
      return;
    }

    const decision = await askIsland({
      hook_event_name: "PermissionRequest",
      cwd: base.cwd,
      session_id: base.session_id,
      tool_name: toolName,
      tool_input: base.tool_input,
    });
    if (decision === "deny") {
      return { block: true, reason: "Denied from the Coucou island" };
    }
    // allow — or no answer at all: the tool runs. Absence never blocks.
    return;
  });

  pi.on("tool_result", (event: Loose, ctx: Loose) => {
    send({
      hook_event_name: event?.isError ? "PostToolUseFailure" : "PostToolUse",
      cwd: cwdOf(ctx),
      session_id: sessionIdOf(ctx),
    });
  });

  // omp's own approval gate (approvalMode write/always-ask, or an explicit
  // `prompt` policy) is waiting at a terminal prompt: mirror it as a question
  // so the island shows that something is pending. Answering stays in the
  // terminal — the relay does not double-ask here (see tool_call above).
  pi.on("tool_approval_requested", (event: Loose, ctx: Loose) => {
    const tool = typeof event?.toolName === "string" ? event.toolName : "a tool";
    send({
      hook_event_name: "Notification",
      cwd: cwdOf(ctx),
      session_id: sessionIdOf(ctx),
      message: `Approve ${tool}?`,
    });
  });

  pi.on("turn_end", (_event: unknown, ctx: Loose) => {
    send({ hook_event_name: "Stop", cwd: cwdOf(ctx), session_id: sessionIdOf(ctx) });
  });

  pi.on("session_shutdown", (_event: unknown, ctx: Loose) => {
    send({ hook_event_name: "SessionEnd", cwd: cwdOf(ctx), session_id: sessionIdOf(ctx) });
  });
}
