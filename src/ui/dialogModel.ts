// Behaviour of the shared modal: focus stays inside while it is open and
// returns to where it was when it closes; Escape closes, Enter confirms.
// The markup is in `Dialog.tsx`.

import { onCleanup, onMount, type JSX } from "solid-js";

const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

export interface DialogProps {
  title: JSX.Element;
  onClose: () => void;
  children?: JSX.Element;
  footer?: JSX.Element;
  alert?: boolean;
  size?: "sm" | "md" | "lg";
  class?: string;
  /// A click on the scrim does not close it (forms that hold typed input).
  persistent?: boolean;
  /// Enter anywhere outside a textarea or button triggers this.
  onEnter?: () => void;
}

export function createDialogModel(props: DialogProps) {
  let el: HTMLDivElement | undefined;
  const before = document.activeElement as HTMLElement | null;

  onMount(() => {
    // an autofocused control inside wins; otherwise the dialog itself
    setTimeout(() => {
      if (el && !el.contains(document.activeElement)) el.focus();
    });
  });
  onCleanup(() => before?.focus?.());

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      props.onClose();
    } else if (e.key === "Enter" && props.onEnter) {
      const t = e.target as HTMLElement;
      if (t.tagName === "TEXTAREA" || t.tagName === "BUTTON" || t.tagName === "SELECT") return;
      e.preventDefault();
      props.onEnter();
    } else if (e.key === "Tab" && el) {
      const items = [...el.querySelectorAll<HTMLElement>(FOCUSABLE)];
      if (items.length === 0) return;
      const first = items[0];
      const last = items[items.length - 1];
      if (e.shiftKey && (document.activeElement === first || document.activeElement === el)) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  };

  const onScrimDown = (e: MouseEvent) => {
    if (e.target !== e.currentTarget) return;
    // keep focus inside, so Escape and Tab still reach the dialog
    e.preventDefault();
    if (!props.persistent) props.onClose();
  };

  return {
    setEl: (node: HTMLDivElement) => (el = node),
    onKeyDown,
    onScrimDown,
  };
}
