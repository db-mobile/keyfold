// State and actions of the jump palette (CONCEPT 5.4). Recently opened
// keys are kept per connection in the browser's local storage (a few
// names, never the keyspace). The markup is in `JumpPalette.tsx`.

import { createMemo, createSignal, onMount } from "solid-js";
import type { KeyRef } from "./api";
import { isBinary, sameKey, textKey } from "./keyRef";

const MAX_RECENT = 20;

export interface JumpPaletteProps {
  scope: string;
  onJump: (key: KeyRef) => void;
  onClose: () => void;
}

export function recentKey(scope: string): string {
  return `keyfold.recent.${scope}`;
}

/// One stored entry: a key reference, or a plain name from before names
/// carried their exact bytes.
function recentFrom(v: unknown): KeyRef | null {
  if (typeof v === "string") return textKey(v);
  if (v && typeof v === "object") {
    const { text, raw } = v as { text?: unknown; raw?: unknown };
    if (typeof text === "string" && (raw === null || typeof raw === "string")) return { text, raw };
  }
  return null;
}

export function loadRecent(scope: string): KeyRef[] {
  try {
    const raw = localStorage.getItem(recentKey(scope));
    const v = raw ? (JSON.parse(raw) as unknown) : [];
    return Array.isArray(v) ? v.map(recentFrom).filter((k): k is KeyRef => k !== null) : [];
  } catch {
    return [];
  }
}

export function pushRecent(scope: string, key: KeyRef): KeyRef[] {
  const next = [key, ...loadRecent(scope).filter((k) => !sameKey(k, key))].slice(0, MAX_RECENT);
  try {
    localStorage.setItem(recentKey(scope), JSON.stringify(next));
  } catch {
    // storage unavailable: recents are a convenience only
  }
  return next;
}

export function createJumpPaletteModel(props: JumpPaletteProps) {
  let input: HTMLInputElement | undefined;
  const [text, setText] = createSignal("");
  const [cursor, setCursor] = createSignal(0);
  const recent = loadRecent(props.scope);

  const options = createMemo(() => {
    const t = text();
    const hits = recent.filter((k) => k.text.includes(t));
    // what is typed is a text name; it leads unless a recent key is exactly it
    return t && !hits.some((k) => sameKey(k, textKey(t))) ? [textKey(t), ...hits] : hits;
  });
  const isRecent = (k: KeyRef) => recent.some((r) => sameKey(r, k));

  const jump = (key: KeyRef) => {
    if (!key.text && key.raw === null) return;
    props.onJump(key);
    props.onClose();
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      props.onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setCursor((c) => Math.min(c + 1, options().length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setCursor((c) => Math.max(c - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      jump(options()[cursor()] ?? textKey(text()));
    }
  };

  onMount(() => input?.focus());

  return {
    text,
    options,
    setInput: (el: HTMLInputElement) => (input = el),
    /// Typing starts the selection at the top again.
    setText: (value: string) => {
      setText(value);
      setCursor(0);
    },
    setCursor: (i: number) => setCursor(i),
    active: (i: number) => i === cursor(),
    jump,
    onKey,
    /// The input keeps the focus for as long as the palette is open.
    refocus: () => input?.focus(),
    onOverlayDown: (e: MouseEvent) => e.target === e.currentTarget && props.onClose(),
    showRecentCaption: () => options().length > 0 && options().some(isRecent) && !text(),
    /// Not among the recent keys: the typed name itself.
    fromServer: (k: KeyRef) => !isRecent(k),
    isBinary,
    empty: () => options().length === 0,
  };
}
