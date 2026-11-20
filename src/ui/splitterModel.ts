// Dragging the line between the sidebar and the value view sets the
// sidebar width, which is remembered across runs. Arrow keys move it too.
// The markup is in `Splitter.tsx`.

import { createSignal } from "solid-js";

const KEY = "keyfold.sidebarWidth";
const MIN = 280;
const DEFAULT = 380;

function stored(): number {
  try {
    const n = Number(localStorage.getItem(KEY));
    return n >= MIN ? n : DEFAULT;
  } catch {
    return DEFAULT;
  }
}

export function createSplitterModel() {
  const [dragging, setDragging] = createSignal(false);
  let width = stored();
  const set = (w: number) => {
    width = Math.max(MIN, Math.min(w, window.innerWidth - 320));
    document.documentElement.style.setProperty("--sidebar-width", `${width}px`);
  };
  const save = () => {
    try {
      localStorage.setItem(KEY, String(Math.round(width)));
    } catch {
      // the width then lasts for this run only
    }
  };
  set(width);

  const onPointerDown = (e: PointerEvent) => {
    e.preventDefault();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    setDragging(true);
  };
  const onPointerMove = (e: PointerEvent) => {
    if (dragging()) set(e.clientX);
  };
  const onPointerUp = () => {
    if (!dragging()) return;
    setDragging(false);
    save();
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    set(width + (e.key === "ArrowLeft" ? -24 : 24));
    save();
  };

  return { dragging, onPointerDown, onPointerMove, onPointerUp, onKeyDown };
}
