// The namespace tree: a virtualised, flattened view over the `nodes`
// table, with one row per node, fold, bucket, list and filter box
// (CONCEPT 5.6). Markup only; state and actions are in `treeModel.ts`.

import { Index, Show, Switch, Match } from "solid-js";
import { createTreeModel, type TreeProps, type TwistyProps } from "./treeModel";
import Icon from "./ui/Icon";
import { MenuItem } from "./ui/Popover";
import Select from "./ui/Select";

export default function Tree(props: TreeProps) {
  const m = createTreeModel(props);
  return (
    <div class="tree">
      <div class="sidebar-caption">
        <span class="dim-label numeric">
          {m.topLevelLabel()}
          {m.delimiterLabel()}
        </span>
        <span class="spacer" />
        <Select compact class="flat" value={m.sort()} onChange={(e) => m.setSort(e.currentTarget.value)} aria-label="Sort">
          <option value="count">By count</option>
          <option value="name">By name</option>
          <option value="numeric">Numeric</option>
        </Select>
      </div>
      <div class="scroll" ref={m.setScrollEl}>
        <div class="inner" style={m.innerStyle()}>
          <Index each={m.items()}>
            {(item) => (
              <Show when={m.row(item())}>
                {(r) => (
                  <Switch>
                    <Match when={r().kind === "node"}>
                      <div class={m.nodeClass(item())} style={m.rowStyle(item())} onClick={() => m.activate(item())} onContextMenu={(e) => m.openMenu(e, item())}>
                        <Twisty show={m.nodeHasChildren(item())} open={m.nodeOpen(item())} />
                        <span class={m.segClass(item())} title={m.nodePrefix(item())}>
                          {m.segLabel(item())}
                          <Show when={m.alsoKey(item())}>
                            <span class="also-key" title="This prefix is also a key" />
                          </Show>
                        </span>
                        <Show when={m.segBinary(item())}>
                          <span class="badge mark">binary</span>
                        </Show>
                        <Show when={m.nodeScan(item())}>
                          <span class="badge mark accent">{m.nodeScan(item())}</span>
                        </Show>
                        <span class="cnt" title={m.childrenTitle(item())}>
                          {m.nodeApprox(item())}
                          {m.nodeCount(item())}
                        </span>
                      </div>
                    </Match>
                    <Match when={r().kind === "more"}>
                      <div class="t" style={m.rowStyle(item())}>
                        <span class="more">{m.moreLabel(r())}</span>
                      </div>
                    </Match>
                    <Match when={r().kind === "folding"}>
                      <div class="t" style={m.rowStyle(item())}>
                        <span class="more">Deciding how to fold…</span>
                      </div>
                    </Match>
                    <Match when={r().kind === "fold"}>
                      {((v) => (
                        <div class="t fold" style={m.rowStyle(item())} onClick={v.toggle}>
                          <Twisty show open={v.open()} />
                          <span class="seg label" title={v.why()}>
                            {v.label()}
                          </span>
                          <span class="dim">
                            {v.range()}
                            {v.samples()}
                          </span>
                          <span class="cnt" title={v.shareTitle()}>
                            {v.members()} ids
                          </span>
                        </div>
                      ))(m.foldView(r()))}
                    </Match>
                    <Match when={r().kind === "summary"}>
                      {((v) => (
                        <div class="t summary" style={m.rowStyle(item())} title={v.title()}>
                          <Twisty show={v.hasChildren()} open={false} />
                          <span class="seg">{v.segment()}</span>
                          <span class="dim">{v.keys()}</span>
                          <span class="cnt">{v.coverage()}</span>
                        </div>
                      ))(m.summaryView(r()))}
                    </Match>
                    <Match when={r().kind === "bucket"}>
                      {((v) => (
                        <div class="t bucket" style={m.rowStyle(item())} onClick={v.toggle}>
                          <Twisty show open={v.open()} />
                          <span class="seg dim">{v.label()}</span>
                          <span class="cnt">{v.count()}</span>
                        </div>
                      ))(m.bucketView(r()))}
                    </Match>
                    <Match when={r().kind === "list"}>
                      {((v) => (
                        <div class="t bucket" style={m.rowStyle(item())} onClick={v.toggle}>
                          <Twisty show open={v.open()} />
                          <span class="seg dim">Show as list</span>
                          <span class="cnt">{v.members()}</span>
                        </div>
                      ))(m.listView(r()))}
                    </Match>
                    <Match when={r().kind === "filter"}>
                      {((v) => (
                        <div class="t" style={m.rowStyle(item())}>
                          <Twisty show={false} open={false} />
                          <input
                            class="entry filter"
                            placeholder="Filter ids by prefix"
                            aria-label="Filter ids by prefix"
                            spellcheck={false}
                            value={v.text()}
                            onInput={(e) => v.setText(e.currentTarget.value)}
                          />
                          <span class="cnt">{v.matches()}</span>
                        </div>
                      ))(m.filterView(r()))}
                    </Match>
                  </Switch>
                )}
              </Show>
            )}
          </Index>
        </div>
      </div>
      <div class="sidebar-caption" title={m.requestsTitle()}>
        <span class="dim-label numeric">{m.keysLabel()}</span>
        <Show when={m.canScanAll()}>
          <span class="spacer" />
          <button class="button flat small" onClick={m.scanAll} title="Read every key name into the index, after a confirmation">
            Scan everything
          </button>
        </Show>
      </div>
      <Show when={m.menu()}>
        {(c) => (
          <div class="context-menu" role="menu" style={m.menuStyle(c())} onClick={m.keepMenu}>
            <div class="menu-title">{m.menuTitle(c())}*</div>
            <MenuItem disabled={!m.canSearchPrefix(c())} onClick={() => m.searchPrefix(c())}>
              Search this prefix
            </MenuItem>
            <Show when={m.canOpenKey(c())}>
              <MenuItem onClick={() => m.openKey(c())}>Open key</MenuItem>
            </Show>
            <MenuItem title={m.scanTitle(c())} onClick={() => m.scanNode(c())}>
              {m.scanLabel(c())}
            </MenuItem>
            <div class="menu-separator" />
            <MenuItem danger onClick={() => m.deletePrefix(c())}>
              Delete {m.approx()}{m.menuCount(c())} keys under it…
            </MenuItem>
          </div>
        )}
      </Show>
    </div>
  );
}

/// The disclosure arrow of a row, or the space it would take.
function Twisty(props: TwistyProps) {
  return (
    <span class="tw" classList={{ open: props.open }}>
      <Show when={props.show}>
        <Icon name="chevron-right" size={12} />
      </Show>
    </span>
  );
}
