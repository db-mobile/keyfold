// Appearance: light, dark or the desktop's setting, and the accent colour.
// Environment colours (dev, staging, prod) do not follow the accent.

import { For } from "solid-js";
import Dialog from "./ui/Dialog";
import { accent, ACCENTS, setAccent, setTheme, theme, type ThemeChoice } from "./ui/theme";

const THEMES: { id: ThemeChoice; label: string }[] = [
  { id: "system", label: "System" },
  { id: "light", label: "Light" },
  { id: "dark", label: "Dark" },
];

export default function Preferences(props: { onClose: () => void }) {
  return (
    <Dialog title="Preferences" size="sm" onClose={props.onClose}>
      <p class="list-group-title">Appearance</p>
      <div class="boxed-list">
        <div class="row">
          <div class="row-content">
            <span class="title">Style</span>
          </div>
          <div class="suffix">
            <div class="view-switcher" role="radiogroup" aria-label="Style">
              <For each={THEMES}>
                {(t) => (
                  <button class="view-switcher-btn" classList={{ active: theme() === t.id }} role="radio" aria-checked={theme() === t.id} onClick={() => setTheme(t.id)}>
                    {t.label}
                  </button>
                )}
              </For>
            </div>
          </div>
        </div>
        <div class="row accent-row">
          <div class="row-content">
            <span class="title">Accent colour</span>
          </div>
          <div class="accent-grid" role="radiogroup" aria-label="Accent colour">
            <For each={ACCENTS}>
              {(a) => (
                <button
                  class="accent-btn"
                  classList={{ active: accent() === a.id }}
                  style={{ "--btn-color": a.color }}
                  role="radio"
                  aria-checked={accent() === a.id}
                  aria-label={a.id}
                  title={a.id}
                  onClick={() => setAccent(a.id)}
                />
              )}
            </For>
          </div>
        </div>
      </div>
    </Dialog>
  );
}
