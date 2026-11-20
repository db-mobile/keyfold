// Adwaita symbolic icons, drawn as CSS masks so they take the text colour.

export type IconName =
  | "alarm"
  | "alert-circle"
  | "check"
  | "chevron-down"
  | "chevron-right"
  | "chevron-up"
  | "gear"
  | "list"
  | "menu"
  | "more"
  | "pause"
  | "pencil"
  | "play"
  | "plus"
  | "refresh"
  | "search"
  | "server"
  | "terminal"
  | "trash"
  | "warning"
  | "x";

export default function Icon(props: { name: IconName; size?: 12 | 14 | 16 | 18 | 20 | 64; class?: string }) {
  return <span class={`icon icon-${props.size ?? 16} icon-${props.name} ${props.class ?? ""}`} aria-hidden="true" />;
}
