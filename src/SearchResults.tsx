// Search results (CONCEPT 5.3): the query is parsed by the backend and the
// mode shown as a chip. Markup only; state and actions are in
// `searchResultsModel.ts`.

import { Index, Show } from "solid-js";
import { createSearchResultsModel, type SearchResultsProps } from "./searchResultsModel";

export default function SearchResults(props: SearchResultsProps) {
  const m = createSearchResultsModel(props);
  return (
    <div class="sidebar-view">
      <div class="search-head">
        <div class="search-summary">
          <span class={m.badgeClass()} title="How the query was read">
            {m.modeText()}
          </span>
          <span class="truncate numeric" title={m.summary()}>
            {m.summary()}
          </span>
          <Show when={m.ftsBusy()}>
            <span class="dim-label numeric">
              {m.ftsPhase()} {m.ftsPercent()}
            </span>
          </Show>
        </div>
        <div class="search-actions">
          <Show when={m.canVerify()}>
            <button class="btn btn-xs" disabled={m.verifying()} onClick={m.runVerify} title="SCAN MATCH on the server, bounded; adds what the index lacks and removes what the server no longer has">
              {m.verifyLabel()}
            </button>
          </Show>
          <Show when={m.canEnableFts()}>
            <button class="btn btn-xs" onClick={m.enableFts} title="Builds a trigram table from the index on disk, no server traffic; 2 to 3 times the index size">
              Enable instant substring search
            </button>
          </Show>
          <Show when={m.canDisableFts()}>
            <button class="btn btn-xs" onClick={m.disableFts}>
              Disable trigram index
            </button>
          </Show>
          <Show when={m.canDeleteAll()}>
            <button class="btn btn-xs destructive-text" onClick={m.deletePattern} title="Exact count and a sample before confirming; batched UNLINK through the index">
              Delete all {m.countText()}…
            </button>
          </Show>
        </div>
        <Show when={m.verifyNote()}>
          {(v) => (
            <p class="search-note dim-label">
              {v().label}: {v().matched} on the server after {v().calls} SCAN calls, {v().added} added,{" "}
              {v().removed} removed{v().truncated}, {v().took} ms
            </p>
          )}
        </Show>
        <Show when={m.error()}>
          <p class="search-note error-text">{m.error()}</p>
        </Show>
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
              <div
                class="keylist-row"
                classList={{ placeholder: m.isPlaceholder(item().index), selected: m.isSelected(item().index) }}
                style={m.rowStyle(item().start)}
                onClick={() => m.select(item().index)}
              >
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
            )}
          </Index>
        </div>
        <Show when={m.noMatches()}>
          <div class="status-page compact">
            <p class="status-page-title">No key matches</p>
            <p class="status-page-description">{m.noMatchesHint()}</p>
          </div>
        </Show>
      </div>
    </div>
  );
}
