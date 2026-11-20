// Short confirmations at the bottom of the window. An action keeps its name:
// the button that says "Rename" produces a toast that says "Renamed".
// The markup is in `Toast.tsx`.

import { createSignal } from "solid-js";

export type ToastKind = "info" | "success" | "error";

export interface ToastItem {
  id: number;
  message: string;
  kind: ToastKind;
  visible: boolean;
}

const DURATION: Record<ToastKind, number> = { info: 4000, success: 3000, error: 6000 };
const [items, setItems] = createSignal<ToastItem[]>([]);
let nextId = 1;

export { items as toasts };

export function toast(message: string, kind: ToastKind = "info") {
  const id = nextId++;
  setItems((list) => [...list.slice(-2), { id, message, kind, visible: false }]);
  requestAnimationFrame(() => setItems((list) => list.map((t) => (t.id === id ? { ...t, visible: true } : t))));
  setTimeout(() => dismissToast(id), DURATION[kind]);
}

export function dismissToast(id: number) {
  setItems((list) => list.map((t) => (t.id === id ? { ...t, visible: false } : t)));
  setTimeout(() => setItems((list) => list.filter((t) => t.id !== id)), 250);
}
