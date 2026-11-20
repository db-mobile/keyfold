// Jump to key (CONCEPT 5.4): Ctrl+K, type an exact key name, Enter opens
// it straight from the server through the value panel's verify-on-open,
// so it works with no index at all. Recently opened keys are listed first.
// Markup only; state, actions and the recent-key storage are in
// `jumpPaletteModel.ts`.

import { For, Show } from "solid-js";
import { createJumpPaletteModel, type JumpPaletteProps } from "./jumpPaletteModel";
import Icon from "./ui/Icon";

export { loadRecent, pushRecent, recentKey } from "./jumpPaletteModel";

export default function JumpPalette(props: JumpPaletteProps) {
  const m = createJumpPaletteModel(props);
  return (
    <div class="modal-overlay palette-overlay" onMouseDown={m.onOverlayDown}>
      <div class="palette" role="dialog" aria-modal="true" aria-label="Jump to key">
        <div class="palette-input">
          <Icon name="search" />
          <input
            ref={m.setInput}
            placeholder="Exact key name"
            aria-label="Key name"
            value={m.text()}
            onInput={(e) => m.setText(e.currentTarget.value)}
            onKeyDown={m.onKey}
            onBlur={m.refocus}
            spellcheck={false}
          />
        </div>
        <div class="palette-options" role="listbox">
          <Show when={m.showRecentCaption()}>
            <p class="palette-caption">Recent keys</p>
          </Show>
          <For each={m.options()}>
            {(k, i) => (
              <div class="palette-option" classList={{ active: m.active(i()) }} role="option" aria-selected={m.active(i())} onMouseEnter={() => m.setCursor(i())} onClick={() => m.jump(k)}>
                <span class="name">{k.text}</span>
                <Show when={m.isBinary(k)}>
                  <span class="badge mark">binary</span>
                </Show>
                <Show when={m.fromServer(k)}>
                  <span class="caption dim-label">Open from the server</span>
                </Show>
              </div>
            )}
          </For>
          <Show when={m.empty()}>
            <p class="palette-caption">Type a key name and press Enter. It opens straight from the server, with or without an index.</p>
          </Show>
        </div>
      </div>
    </div>
  );
}
