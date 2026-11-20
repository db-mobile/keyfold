// State and actions of the command console (CONCEPT section 8). The markup
// is in `Console.tsx`. One line is typed, the backend decides whether it
// runs, is refused or needs a confirmation, and the reply comes back as
// lines that are already cut to a bound. The scrollback holds at most 5,000
// lines; the history of typed lines lives in memory for this session only,
// because a typed line can hold a value.

import { createVirtualizer, type VirtualItem } from "@tanstack/solid-virtual";
import { createEffect, createSignal, on, onMount } from "solid-js";
import { api, type Connection, type ConsoleReply } from "./api";
import { ask } from "./dialogModel";
import { VALUE_ROW } from "./ui/density";

const ROW = VALUE_ROW;
const MAX_LINES = 5000;
const MAX_HISTORY = 200;
const HEIGHT_KEY = "keyfold.consoleHeight";
const MIN_HEIGHT = 120;
const DEFAULT_HEIGHT = 240;

export type LineKind = "cmd" | "out" | "err" | "note";
export interface Line {
  kind: LineKind;
  text: string;
}

export interface ConsoleProps {
  conn: Connection;
  onClose: () => void;
  /// The connection's numbers may have changed (a write, a flush).
  onRefresh: () => void;
  /// Development aid: run these lines on opening, answering every
  /// confirmation, and log the replies.
  autorun?: string[];
  onAutorunDone?: () => void;
}

function storedHeight(): number {
  try {
    const n = Number(localStorage.getItem(HEIGHT_KEY));
    return n >= MIN_HEIGHT ? n : DEFAULT_HEIGHT;
  } catch {
    return DEFAULT_HEIGHT;
  }
}

export function createConsoleModel(props: ConsoleProps) {
  let scrollEl: HTMLDivElement | undefined;
  let inputEl: HTMLInputElement | undefined;
  const [lines, setLines] = createSignal<Line[]>([]);
  const [input, setInput] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [node, setNode] = createSignal("");
  const [dragging, setDragging] = createSignal(false);
  const history: string[] = [];
  /// Position in the history while browsing it with the arrow keys.
  let at = -1;
  let draft = "";

  let height = storedHeight();
  const setHeight = (h: number) => {
    height = Math.max(MIN_HEIGHT, Math.min(h, window.innerHeight - 220));
    document.documentElement.style.setProperty("--console-height", `${height}px`);
  };
  const saveHeight = () => {
    try {
      localStorage.setItem(HEIGHT_KEY, String(Math.round(height)));
    } catch {
      // the height then lasts for this run only
    }
  };
  setHeight(height);

  const virtualizer = createVirtualizer({
    get count() {
      return lines().length;
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => ROW,
    overscan: 12,
  });

  const append = (more: Line[]) => {
    const all = [...lines(), ...more];
    setLines(all.length > MAX_LINES ? all.slice(all.length - MAX_LINES) : all);
  };

  // What was just printed is what the user wants to see.
  createEffect(
    on(lines, (l) => {
      if (l.length > 0) queueMicrotask(() => virtualizer.scrollToIndex(l.length - 1, { align: "end" }));
    }),
  );

  const show = (r: ConsoleReply) => {
    const kind: LineKind = r.status === "ok" ? "out" : "err";
    const out: Line[] = r.lines.map((text) => ({ kind, text }));
    if (r.status === "refused" && out.length > 0) out[0] = { kind: "err", text: `Not sent: ${out[0].text}` };
    const facts = [r.node, r.status === "refused" ? null : `${r.tookMs.toFixed(1)} ms`, r.truncated ? "reply cut" : null, r.indexNote].filter(Boolean);
    if (facts.length > 0) out.push({ kind: "note", text: facts.join(", ") });
    append(out);
  };

  const run = async (auto = false) => {
    const line = input().trim();
    if (!line || busy()) return;
    if (history[history.length - 1] !== line) history.push(line);
    if (history.length > MAX_HISTORY) history.shift();
    at = -1;
    setInput("");
    append([{ kind: "cmd", text: `${node() ? `${node()} ` : ""}> ${line}` }]);
    setBusy(true);
    try {
      let r = await api.consoleRun(props.conn.id, line, node() || null, null);
      if (r.status === "confirm" && r.confirm) {
        const c = r.confirm;
        if (auto) void api.log("info", `autoconsole ${line}: asks (${c.level}) ${c.text}`);
        const ok = auto || await ask({
          title: c.level === "write" ? "Run this on a prod connection?" : "Run this command?",
          body: `${c.text}\n\n${line}`,
          confirm: "Run it",
          danger: true,
          typeToConfirm: c.expect ?? undefined,
        });
        if (!ok) {
          append([{ kind: "note", text: "Not run." }]);
          return;
        }
        r = await api.consoleRun(props.conn.id, line, node() || null, c.expect ?? "yes");
      }
      show(r);
      if (auto) void api.log("info", `autoconsole ${line}: ${r.status} ${JSON.stringify(r.lines.slice(0, 6))}${r.truncated ? " (cut)" : ""} ${r.node ?? ""} ${r.indexNote ?? ""}`);
      if (r.indexNote) props.onRefresh();
    } catch (e) {
      append([{ kind: "err", text: String(e) }]);
      if (auto) void api.log("error", `autoconsole ${line}: ${e}`);
    } finally {
      setBusy(false);
      inputEl?.focus();
    }
  };

  const recall = (step: number) => {
    if (history.length === 0) return;
    if (at === -1) {
      if (step > 0) return;
      draft = input();
      at = history.length - 1;
    } else {
      at += step;
    }
    if (at >= history.length) {
      at = -1;
      setInput(draft);
      return;
    }
    at = Math.max(0, at);
    setInput(history[at]);
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void run();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      recall(-1);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      recall(1);
    } else if (e.key === "Escape") {
      e.preventDefault();
      props.onClose();
    } else if (e.key.toLowerCase() === "l" && e.ctrlKey) {
      e.preventDefault();
      setLines([]);
    }
  };

  onMount(async () => {
    inputEl?.focus();
    if (!props.autorun?.length) return;
    for (const line of props.autorun) {
      setInput(line);
      await run(true);
    }
    props.onAutorunDone?.();
  });

  /// The nodes a command without keys can be sent to.
  const primaries = () => props.conn.nodes.filter((n) => n.role === "primary" && n.online).map((n) => n.addr);

  return {
    input,
    busy,
    node,
    setInput: (value: string) => setInput(value),
    setNode: (value: string) => setNode(value),
    setScrollEl: (el: HTMLDivElement) => (scrollEl = el),
    setInputEl: (el: HTMLInputElement) => (inputEl = el),
    focusInput: () => inputEl?.focus(),
    onKeyDown,
    clear: () => setLines([]),
    close: () => props.onClose(),

    // rows
    items: () => virtualizer.getVirtualItems(),
    innerStyle: () => ({ height: `${virtualizer.getTotalSize()}px` }),
    rowStyle: (item: VirtualItem) => ({ transform: `translateY(${item.start}px)` }),
    rowClass: (item: VirtualItem) => `console-line ${lines()[item.index]?.kind ?? "out"}`,
    rowText: (item: VirtualItem) => lines()[item.index]?.text ?? "",
    empty: () => lines().length === 0,

    // the node of a cluster a command without keys goes to
    isCluster: () => props.conn.cluster !== null,
    primaries,
    prompt: () => (busy() ? "…" : ">"),
    placeholder: () => (props.conn.tag === "prod" ? "A command. Writes ask first on this connection." : "A command, as in redis-cli"),

    // the line above the panel resizes it
    dragging,
    onPointerDown: (e: PointerEvent) => {
      e.preventDefault();
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      setDragging(true);
    },
    onPointerMove: (e: PointerEvent) => {
      // the panel ends above the status bar, whose height is 26 px and a border
      if (dragging()) setHeight(window.innerHeight - e.clientY - 27);
    },
    onPointerUp: () => {
      if (!dragging()) return;
      setDragging(false);
      saveHeight();
    },
    onResizeKey: (e: KeyboardEvent) => {
      if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
      e.preventDefault();
      setHeight(height + (e.key === "ArrowUp" ? 24 : -24));
      saveHeight();
    },
  };
}
