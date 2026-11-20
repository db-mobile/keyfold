// A popover anchored to its trigger, and the entries of a popover menu.
// Markup only; behaviour is in `popoverModel.ts`.

import { Show } from "solid-js";
import { createPopoverModel, type MenuItemProps, type PopoverProps } from "./popoverModel";

export type { PopoverControl } from "./popoverModel";

export default function Popover(props: PopoverProps) {
  const m = createPopoverModel(props);
  return (
    <span class="popover-anchor" ref={m.setAnchor}>
      {props.trigger(m.control)}
      <Show when={m.open()}>
        <div class={`popover ${props.class ?? ""}`} classList={{ "popover-end": props.align === "end", "popover-up": props.up }} role="dialog">
          {props.children(m.close)}
        </div>
      </Show>
    </span>
  );
}

/// One entry of a popover menu.
export function MenuItem(props: MenuItemProps) {
  return (
    <button class="menu-item" classList={{ destructive: props.danger }} role="menuitem" onClick={props.onClick} disabled={props.disabled} title={props.title}>
      {props.children}
      <Show when={props.accel}>
        <span class="menu-item-accel">{props.accel}</span>
      </Show>
    </button>
  );
}
