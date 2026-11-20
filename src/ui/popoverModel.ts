// Behaviour of a popover: it closes on Escape and on a click outside, and
// hands focus back to the trigger. The markup is in `Popover.tsx`.

import { createSignal, onCleanup, type JSX } from "solid-js";

export interface PopoverControl {
  open: () => boolean;
  toggle: () => void;
}

export interface PopoverProps {
  trigger: (ctl: PopoverControl) => JSX.Element;
  children: (close: () => void) => JSX.Element;
  align?: "start" | "end";
  /// Open above the trigger instead of below it.
  up?: boolean;
  class?: string;
  onOpen?: () => void;
}

export interface MenuItemProps {
  onClick: () => void;
  children: JSX.Element;
  accel?: string;
  danger?: boolean;
  disabled?: boolean;
  title?: string;
}

export function createPopoverModel(props: PopoverProps) {
  const [open, setOpen] = createSignal(false);
  let anchor: HTMLSpanElement | undefined;

  const close = (refocus = true) => {
    if (!open()) return;
    setOpen(false);
    if (refocus) anchor?.querySelector<HTMLElement>("button, [tabindex]")?.focus();
  };
  const toggle = () => {
    if (open()) close();
    else {
      setOpen(true);
      props.onOpen?.();
    }
  };

  const onDown = (e: MouseEvent) => {
    if (open() && anchor && !anchor.contains(e.target as Node)) close(false);
  };
  const onKey = (e: KeyboardEvent) => {
    if (open() && e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };
  document.addEventListener("mousedown", onDown, true);
  document.addEventListener("keydown", onKey, true);
  onCleanup(() => {
    document.removeEventListener("mousedown", onDown, true);
    document.removeEventListener("keydown", onKey, true);
  });

  const control: PopoverControl = { open, toggle };
  return {
    open,
    control,
    close: () => close(),
    setAnchor: (node: HTMLSpanElement) => (anchor = node),
  };
}
