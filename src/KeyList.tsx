// The flat list: a virtualised view over windows of 200 rows served by the
// backend. The DOM holds about 60 rows. Markup only; state and actions are
// in `keyListModel.ts`.

import { Index, Show } from "solid-js";
import { createFlatListModel, createKeyListModel, type FlatListProps, type KeyListProps } from "./keyListModel";
import ScanList from "./ScanList";
import SearchResults from "./SearchResults";
import Icon from "./ui/Icon";

export type { NavView } from "./keyListModel";

/// The navigation pane: a view switch and a search box over the tree, the
/// flat list or the search results. A query shows results in either view;
/// clearing it returns to the chosen one. The tree stays mounted while it
/// is hidden so its expanded nodes survive a search or a look at the list.
/// Enter on a plain term opens that exact key (CONCEPT 5.3, jump).
export default function KeyList(props: KeyListProps) {
  const m = createKeyListModel(props);
  return (
    <aside class="sidebar">
      <div class="sidebar-toolbar">
        <Show when={m.noIndex()}>
          <span class="sidebar-title">Keys</span>
        </Show>
        <div class="view-switcher" role="tablist" aria-label="View" hidden={m.noIndex()}>
          <button
            class="view-switcher-btn"
            classList={{ active: m.isTree() }}
            role="tab"
            aria-selected={m.isTree()}
            onClick={m.showTree}
            title="Keys grouped by delimiter"
          >
            Tree
          </button>
          <button
            class="view-switcher-btn"
            classList={{ active: m.isList() }}
            role="tab"
            aria-selected={m.isList()}
            onClick={m.showList}
            title="All keys as one sorted list"
          >
            List
          </button>
        </div>
        <span class="spacer" />
        <button class="button flat image-button small" onClick={props.onNewKey} title="New key" aria-label="New key">
          <Icon name="plus" />
        </button>
      </div>
      <div class="sidebar-toolbar">
        <div class="search-entry">
          <Icon name="search" />
          <input
            class="entry compact monospace"
            type="text"
            placeholder={m.searchPlaceholder()}
            title={m.searchTitle()}
            aria-label="Search keys"
            value={props.query}
            onInput={(e) => props.onQuery(e.currentTarget.value)}
            onKeyDown={m.onSearchKey}
            spellcheck={false}
          />
          <Show when={props.query}>
            <button class="btn-icon search-clear" onClick={m.clearQuery} title="Clear search" aria-label="Clear search">
              <Icon name="x" size={12} />
            </button>
          </Show>
        </div>
      </div>
      <Show when={m.hasIndex()} fallback={<ScanList conn={props.conn} query={m.trimmedQuery()} selected={props.selected} onSelect={props.onSelect} />}>
        <div class="sidebar-view" style={m.treeStyle()}>
          {props.tree}
        </div>
      </Show>
      <Show when={m.listVisible()}>
        <FlatList {...props} />
      </Show>
      <Show when={m.searching()}>
        <SearchResults
          conn={props.conn}
          query={m.trimmedQuery()}
          scanning={m.scanning()}
          selected={props.selected}
          onSelect={props.onSelect}
          version={props.version}
          onDeletePattern={props.onDeletePattern}
        />
      </Show>
      {props.footer}
    </aside>
  );
}

function FlatList(props: FlatListProps) {
  const m = createFlatListModel(props);
  return (
    <div class="sidebar-view">
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
      </div>
      <div class="sidebar-caption" title={m.cacheTitle()}>
        <span class="dim-label numeric">
          {m.approxMark()}
          {m.totalText()} keys
        </span>
      </div>
    </div>
  );
}
