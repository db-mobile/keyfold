// A native select dressed as a GNOME drop-down button.

import { splitProps, type JSX } from "solid-js";
import Icon from "./Icon";

export default function Select(props: JSX.SelectHTMLAttributes<HTMLSelectElement> & { compact?: boolean }) {
  const [own, rest] = splitProps(props, ["compact", "class", "children"]);
  return (
    <span class={`select-wrap ${own.class ?? ""}`}>
      <select class="select" classList={{ compact: own.compact }} {...rest}>
        {own.children}
      </select>
      <Icon name="chevron-down" size={12} class="select-arrow" />
    </span>
  );
}
