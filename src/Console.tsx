// The command console (CONCEPT section 8): a panel above the status bar
// where a command is typed and its reply read, on a connection of its own.
// Markup only; state and actions are in `consoleModel.ts`.

import { For, Index, Show } from "solid-js";
import { createConsoleModel, type ConsoleProps } from "./consoleModel";
import Icon from "./ui/Icon";
import Select from "./ui/Select";

export default function Console(props: ConsoleProps) {
  const m = createConsoleModel(props);
  return (
    <>
      <div
        class="pane-resizer pane-resizer-row"
        classList={{ dragging: m.dragging() }}
        role="separator"
        aria-orientation="horizontal"
        aria-label="Resize console"
        tabindex="0"
        onPointerDown={m.onPointerDown}
        onPointerMove={m.onPointerMove}
        onPointerUp={m.onPointerUp}
        onPointerCancel={m.onPointerUp}
        onKeyDown={m.onResizeKey}
      />
      <section class="console" aria-label="Command console">
        <div class="console-scroll" ref={m.setScrollEl} onClick={m.focusInput}>
          <Show when={m.empty()}>
            <p class="console-hint dim-label">
              Commands run on a connection of their own. Ones that change connection state, block or stream are not sent; dangerous ones ask first.
            </p>
          </Show>
          <div class="console-inner" style={m.innerStyle()}>
            <Index each={m.items()}>
              {(item) => (
                <div class={m.rowClass(item())} style={m.rowStyle(item())}>
                  {m.rowText(item())}
                </div>
              )}
            </Index>
          </div>
        </div>
        <div class="console-input">
          <span class="console-prompt" aria-hidden="true">
            {m.prompt()}
          </span>
          <input
            ref={m.setInputEl}
            class="entry compact monospace"
            type="text"
            placeholder={m.placeholder()}
            aria-label="Command"
            value={m.input()}
            onInput={(e) => m.setInput(e.currentTarget.value)}
            onKeyDown={m.onKeyDown}
            spellcheck={false}
            autocomplete="off"
          />
          <Show when={m.isCluster()}>
            <Select compact value={m.node()} onChange={(e) => m.setNode(e.currentTarget.value)} aria-label="Node for commands without keys" title="A command with keys goes to the node that owns them. One without keys goes here.">
              <option value="">Without keys: the seed</option>
              <For each={m.primaries()}>{(addr) => <option value={addr}>{addr}</option>}</For>
            </Select>
          </Show>
          <button class="button flat small" onClick={m.clear} title="Clear the output (Ctrl+L)">
            Clear
          </button>
          <button class="btn-icon" onClick={m.close} title="Close the console (Esc)" aria-label="Close the console">
            <Icon name="x" />
          </button>
        </div>
      </section>
    </>
  );
}
