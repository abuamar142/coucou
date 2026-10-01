// coucou-relay v1 — Coucou's omp hook. Forwards session events to the island
// over a Unix socket; nothing ever blocks the session.
//
// Wire: one JSON line per event shaped like a Claude Code hook payload
// ({ "hook_event_name": ... }) — the island front end speaks that shape
// natively, so no per-agent special casing exists anywhere.
//
// Budgets: 300 ms to connect. Coucou closed → the connect fails immediately
// (ENOENT/ECONNREFUSED) → one dropped event, omp carries on untouched. Same
// rule as the old coucou-hook.exe: never make the agent wait for us.
//
// Approval is monitor-only for now: `tool_approval_requested` is surfaced as a
// question so the island can show that omp is waiting, while the actual
// decision stays in the terminal. Wiring the island's Allow/Deny back into the
// tool gate is a separate change and deliberately absent here — a relay that
// pretends to answer would be worse than one that does not.
//
// Installed to ~/.omp/agent/hooks/post/coucou-relay.ts by the Coucou app.

import { connect } from "node:net";
import { tmpdir } from "node:os";

const CONNECT_BUDGET_MS = 300;
/** Longest string forwarded for any single field; the island truncates far less. */
const MAX_FIELD_LEN = 2_000;
/** Fields that are pointless to forward and can be enormous. */
const DROPPED_FIELDS = ["tool_response", "transcript_path", "content"];

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

// Both extractors below are called from all seven event handlers and must stay
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

  pi.on("tool_call", (event: Loose, ctx: Loose) => {
    send({
      hook_event_name: "PreToolUse",
      cwd: cwdOf(ctx),
      session_id: sessionIdOf(ctx),
      tool_name: typeof event?.toolName === "string" ? event.toolName : "tool",
      tool_input: event?.input && typeof event.input === "object" ? event.input : {},
    });
  });

  pi.on("tool_result", (event: Loose, ctx: Loose) => {
    send({
      hook_event_name: event?.isError ? "PostToolUseFailure" : "PostToolUse",
      cwd: cwdOf(ctx),
      session_id: sessionIdOf(ctx),
    });
  });

  // omp is waiting at its own approval prompt: show it as a question. The
  // terminal stays the place the answer is given (see header).
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
