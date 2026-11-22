// The value panel (CONCEPT section 6): key facts, the editors for one
// write each, and the virtualised rows of the value. Markup only; state
// and actions are in `valuePanelModel.ts`.

import { Index, Match, Show, Switch } from "solid-js";
import Icon from "./ui/Icon";
import { createValuePanelModel, type ValuePanelProps } from "./valuePanelModel";

export default function ValuePanel(props: ValuePanelProps) {
  const m = createValuePanelModel(props);
  return (
    <main class="main-view value">
      <Show
        when={props.selected}
        fallback={
          <div class="status-page">
            <Icon name="search" size={64} class="status-page-icon" />
            <p class="status-page-title">No key selected</p>
            <p class="status-page-description">Pick a key in the sidebar, or press Ctrl+K and type its exact name.</p>
          </div>
        }
      >
        <div class="value-head">
          <div class="value-title">
            <h1 class="value-key" title={m.keyText()}>
              {m.keyText()}
            </h1>
            <Show when={m.keyBinary()}>
              <span class="badge mark" title="This name is not valid UTF-8; other bytes are shown as \xNN">
                binary
              </span>
            </Show>
            <Show when={m.exists()}>
              <div class="value-actions">
                <button class="button flat small" disabled={m.busy()} onClick={m.rename}>
                  Rename
                </button>
                <button class="button flat small" disabled={m.busy()} onClick={m.editExpiry}>
                  Expiry
                </button>
                <Show when={m.canEditString()}>
                  <button class="button flat small" disabled={m.busy()} onClick={m.editString}>
                    Edit
                  </button>
                </Show>
                <Show when={m.isCollection()}>
                  <button class="button flat small" disabled={m.busy()} onClick={m.add}>
                    Add {m.addWhat()}
                  </button>
                </Show>
                <button class="button flat image-button small" disabled={!m.canReload()} onClick={m.reload} title="Reload from the server" aria-label="Reload from the server">
                  <Icon name="refresh" />
                </button>
                <button class="button flat image-button small destructive-text" disabled={m.busy()} onClick={m.deleteKey} title="Delete key" aria-label="Delete key">
                  <Icon name="trash" />
                </button>
              </div>
            </Show>
          </div>
          <Show when={m.info()}>
            {(i) => (
              <div class="value-facts">
                <Show when={!i().exists}>
                  <span class="badge error">Gone, removed from the index</span>
                </Show>
                <Show when={i().exists}>
                  <span class="badge accent">{i().keyType}</span>
                  <Show when={i().encoding}>
                    <span>{i().encoding}</span>
                  </Show>
                  <span>{m.expiry(i())}</span>
                  <Show when={m.hasMem(i())}>
                    <span>{m.mem(i())}</span>
                  </Show>
                  <span class="spacer" />
                  <span>{m.showing(i())}</span>
                </Show>
              </div>
            )}
          </Show>
          <Show when={m.error()}>
            <p class="error-text value-error">{m.error()}</p>
          </Show>
        </div>
        <Show when={m.editor()}>
          {(ed) => (
            <div class="value-editor" onKeyDown={m.onEditorKey}>
              <Switch>
                <Match when={m.editorIs(ed(), "rename")}>
                  <label class="form-group">
                    <span class="form-label">New name</span>
                    <input class="entry monospace" placeholder={m.renameHint()} value={m.fieldA()} onInput={(e) => m.setFieldA(e.currentTarget.value)} spellcheck={false} ref={m.focusLater} />
                  </label>
                </Match>
                <Match when={m.editorIs(ed(), "ttl")}>
                  <label class="form-group">
                    <span class="form-label">Expire in seconds</span>
                    <input class="entry monospace" value={m.fieldA()} onInput={(e) => m.setFieldA(e.currentTarget.value)} spellcheck={false} ref={m.focusLater} />
                  </label>
                </Match>
                <Match when={m.editorIs(ed(), "string")}>
                  <textarea class="entry monospace" rows={8} value={m.fieldA()} onInput={(e) => m.setFieldA(e.currentTarget.value)} spellcheck={false} aria-label="Value" ref={m.focusLater} />
                </Match>
                <Match when={m.editorIs(ed(), "add")}>
                  <label class="form-group">
                    <span class="form-label">{m.addLabel()}</span>
                    <input class="entry monospace" value={m.fieldA()} onInput={(e) => m.setFieldA(e.currentTarget.value)} spellcheck={false} ref={m.focusLater} />
                  </label>
                  <Show when={m.addHasValue()}>
                    <label class="form-group">
                      <span class="form-label">{m.addValueLabel()}</span>
                      <input class="entry monospace" value={m.fieldB()} onInput={(e) => m.setFieldB(e.currentTarget.value)} spellcheck={false} />
                    </label>
                  </Show>
                  <Show when={m.isList()}>
                    <label class="check-row">
                      <input type="checkbox" class="check" checked={m.head()} onChange={(e) => m.setHead(e.currentTarget.checked)} /> Add at the head
                    </label>
                  </Show>
                </Match>
                <Match when={m.editorIs(ed(), "row")}>
                  <label class="form-group">
                    <span class="form-label">{m.rowLabel(ed())}</span>
                    <input class="entry monospace" value={m.fieldB()} onInput={(e) => m.setFieldB(e.currentTarget.value)} spellcheck={false} ref={m.focusLater} />
                  </label>
                </Match>
              </Switch>
              <div class="value-editor-actions">
                <Show when={m.canPersist(ed())}>
                  <button class="button small" disabled={m.busy()} onClick={m.persist}>
                    Remove expiry
                  </button>
                </Show>
                <span class="spacer" />
                <button class="button small" onClick={m.cancel}>
                  Cancel
                </button>
                <button class="button small suggested-action" disabled={m.busy()} onClick={m.save}>
                  {m.saveLabel()}
                </button>
              </div>
            </div>
          )}
        </Show>
        <Show when={m.filterable()}>
          <div class="value-tools">
            <div class="search-entry">
              <Icon name="search" />
              <input
                class="entry compact monospace"
                type="text"
                placeholder="Filter fields, for example user:*"
                aria-label="Filter fields"
                value={m.filter()}
                onChange={(e) => m.setFilter(e.currentTarget.value)}
                spellcheck={false}
              />
            </div>
            <Show when={m.isZset()}>
              <label class="check-row">
                <input type="checkbox" class="check" checked={m.byRank()} onChange={(e) => m.setByRank(e.currentTarget.checked)} /> By rank
              </label>
            </Show>
          </div>
        </Show>
        <Show when={m.canLoadFull()}>
          <div class="value-tools">
            <button class="button small" onClick={m.loadFull} disabled={m.loading()}>
              {m.loadFullLabel()}
            </button>
            <Show when={m.capped()}>
              <span class="caption dim-label">Capped at {m.cap()}</span>
            </Show>
          </div>
        </Show>
        <div class="vscroll" ref={m.setScrollEl}>
          <div class="vinner" style={m.innerStyle()}>
            <Index each={m.virtualItems()}>
              {(item) => (
                <Show when={m.rowAt(item())}>
                  {(r) => (
                    <Switch>
                      <Match when={m.rowIs(r(), "entry")}>
                        <div class="vr" style={m.rowStyle(item())}>
                          <span class="f" title={m.entry(r()).field}>
                            {m.entry(r()).field}
                          </span>
                          <span class="v" title={m.entry(r()).value}>
                            {m.score(r())}
                            {m.entry(r()).value}
                          </span>
                          <span class="act">
                            <Show when={m.rowEditable()}>
                              <button
                                class="btn-icon"
                                title="Edit"
                                aria-label="Edit"
                                disabled={m.busy()}
                                onClick={() => m.editRow(item(), r())}
                              >
                                <Icon name="pencil" size={12} />
                              </button>
                            </Show>
                            <button class="btn-icon" title="Remove" aria-label="Remove" disabled={m.busy()} onClick={() => m.removeRow(item(), r())}>
                              <Icon name="x" size={12} />
                            </button>
                          </span>
                        </div>
                      </Match>
                      <Match when={m.rowIs(r(), "line")}>
                        <div class="vr line" style={m.rowStyle(item())}>
                          {m.lineText(r())}
                        </div>
                      </Match>
                      <Match when={m.rowIs(r(), "hex")}>
                        <div class="vr hex" style={m.rowStyle(item())}>
                          <span>{m.hexOffset(r())}</span>
                          <span>{m.hex(r())}</span>
                          <span class="ascii">{m.ascii(r())}</span>
                        </div>
                      </Match>
                      <Match when={m.rowIs(r(), "more")}>
                        <div class="vr line dim" style={m.rowStyle(item())}>
                          {m.moreLabel()}
                        </div>
                      </Match>
                    </Switch>
                  )}
                </Show>
              )}
            </Index>
          </div>
        </div>
      </Show>
    </main>
  );
}
