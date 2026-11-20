// Confirmations (CONCEPT section 8). `ask` returns a promise so callers
// read like a sentence; one host (`dialog.tsx`) renders the dialog in
// flight. A prod-tagged connection confirms every write through
// `confirmWrite`; other tags only confirm what deletes a key or more.

import { createSignal, type JSX } from "solid-js";
import type { Connection } from "./api";

export interface Ask {
  title: string;
  body?: JSX.Element | string;
  /// Label of the confirming button.
  confirm: string;
  danger?: boolean;
  /// The user must type this exact text before the button enables.
  typeToConfirm?: string;
}

interface Pending {
  ask: Ask;
  resolve: (ok: boolean) => void;
}

const [pending, setPending] = createSignal<Pending | null>(null);

export function ask(a: Ask): Promise<boolean> {
  pending()?.resolve(false);
  return new Promise((resolve) => setPending({ ask: a, resolve }));
}

/// Prod confirms every write; elsewhere the caller decides what to confirm.
export function confirmWrite(conn: Connection, what: string): Promise<boolean> {
  if (conn.tag !== "prod") return Promise.resolve(true);
  return ask({
    title: `${conn.label} is tagged prod`,
    body: what,
    confirm: "Run it",
    danger: true,
  });
}

export function createDialogHostModel() {
  const [typed, setTyped] = createSignal("");
  const finish = (ok: boolean) => {
    const p = pending();
    setPending(null);
    setTyped("");
    p?.resolve(ok);
  };
  const ready = () => {
    const p = pending();
    return !!p && (!p.ask.typeToConfirm || typed() === p.ask.typeToConfirm);
  };
  const focusLater = (el: HTMLElement) => setTimeout(() => el.focus());
  return {
    /// The question in flight, if any.
    asked: () => pending()?.ask ?? null,
    typed,
    setTyped,
    ready,
    confirm: () => finish(true),
    cancel: () => finish(false),
    confirmIfReady: () => ready() && finish(true),
    focusLater,
    /// The confirming button takes focus unless a text must be typed first.
    focusConfirm: (el: HTMLElement) => !pending()?.ask.typeToConfirm && focusLater(el),
  };
}
