// The key list of a connection without an index (CONCEPT 7, "no index"):
// pages of plain SCAN, appended as the user asks for them through the row
// at the end. Markup only; state and actions are in `scanListModel.ts`.

import { Index, Show } from "solid-js";
import { createScanListModel, type ScanListProps } from "./scanListModel";

export default function ScanList(props: ScanListProps) {
  const m = createScanListModel(props);
  return (
    <div class="sidebar-view">
      <div class="sidebar-caption" title={m.noteTitle()}>
        <span class="dim-label">{m.note()}</span>
      </div>
      <div class="keylist-head">
        <span>Key</span>
        <span>Type</span>
        <span class="num">TTL</span>
        <span class="num">Memory</span>
      </div>
      <div class="keylist-scroll" ref={m.setScrollEl}>
        <div class="keylist-inner" style={m.innerStyle()}>
          <Index each={m.items()}>
            {(item) => (
              <Show
                when={m.isAction(item().index)}
                fallback={
                  <div class="keylist-row" classList={{ selected: m.isSelected(item().index) }} style={m.rowStyle(item().start)} onClick={() => m.select(item().index)}>
                    <span class="name" title={m.rowTitle(item().index)}>
                      <span class="truncate">{m.rowName(item().index)}</span>
                      <Show when={m.isBinary(item().index)}>
                        <span class="badge mark">binary</span>
                      </Show>
                    </span>
                    <span class="dim">{m.rowType(item().index)}</span>
                    <span class="dim num">{m.rowTtl(item().index)}</span>
                    <span class="dim num">{m.rowMem(item().index)}</span>
                  </div>
                }
              >
                <div class="keylist-action" style={m.rowStyle(item().start)}>
                  <Show when={m.loading()}>
                    <span class="dim-label">Reading…</span>
                  </Show>
                  <Show when={m.canLoadMore()}>
                    <button class="button flat small" onClick={m.loadMore}>
                      {m.loadMoreLabel()}
                    </button>
                  </Show>
                  <Show when={m.canProceed()}>
                    <button class="button flat small" onClick={m.proceed}>
                      {m.proceedLabel()}
                    </button>
                  </Show>
                  <Show when={m.atEnd()}>
                    <span class="dim-label">{m.endLabel()}</span>
                  </Show>
                  <Show when={m.canRetry()}>
                    <button class="button flat small" onClick={m.retry}>
                      Try again
                    </button>
                  </Show>
                </div>
              </Show>
            )}
          </Index>
        </div>
      </div>
      <Show when={m.error()}>
        <p class="error-text sidebar-error" role="alert">
          {m.error()}
        </p>
      </Show>
      <div class="sidebar-caption" title={m.captionTitle()}>
        <span class="dim-label numeric">{m.caption()}</span>
        <span class="spacer" />
        <button class="button flat small" onClick={m.restart} title="Read the keys from the beginning again">
          Start over
        </button>
      </div>
    </div>
  );
}
