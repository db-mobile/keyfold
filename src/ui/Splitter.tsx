// The line between the sidebar and the value view. Markup only; dragging
// and the remembered width are in `splitterModel.ts`.

import { createSplitterModel } from "./splitterModel";

export default function Splitter() {
  const m = createSplitterModel();
  return (
    <div
      class="pane-resizer pane-resizer-col"
      classList={{ dragging: m.dragging() }}
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      tabindex="0"
      onPointerDown={m.onPointerDown}
      onPointerMove={m.onPointerMove}
      onPointerUp={m.onPointerUp}
      onPointerCancel={m.onPointerUp}
      onKeyDown={m.onKeyDown}
    />
  );
}
