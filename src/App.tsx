// The application shell: the connection manager, or the open connection
// with its header, panes and dialogs. Markup only; state, actions and the
// startup automation are in `appModel.ts`.

import { Show } from "solid-js";
import { createAppModel } from "./appModel";
import Connections from "./Connections";
import Console from "./Console";
import Header from "./Header";
import KeyList from "./KeyList";
import Tree from "./Tree";
import ValuePanel from "./ValuePanel";
import JumpPalette from "./JumpPalette";
import DialogHost from "./dialog";
import DeleteDialog from "./DeleteDialog";
import FlushDialog from "./FlushDialog";
import NewKeyDialog from "./NewKeyDialog";
import Preferences from "./Preferences";
import ScanFooter from "./ScanFooter";
import Splitter from "./ui/Splitter";
import ToastHost from "./ui/Toast";

export default function App() {
  const m = createAppModel();
  return (
    <>
      <Show when={m.conn()} fallback={<Connections onConnect={m.connect} onConnectProfile={m.connectProfile} error={m.error()} onPreferences={m.openPrefs} />}>
        {(c) => {
          const scan = m.createScan(c);
          return (
            <div class="app">
              <Header
                conn={c()}
                scan={scan}
                onDisconnect={m.disconnect}
                onSwitchDb={m.switchDb}
                onFlush={m.openFlush}
                onRefresh={m.refresh}
                onNewKey={m.openNewKey}
                onJump={m.openPalette}
                onConsole={m.toggleConsole}
                consoleOpen={m.consoleOpen()}
                onPreferences={m.openPrefs}
              />
              <div class="app-body">
                {/* a database switch replaces the session: the panes start over */}
                <Show when={c().id} keyed>
                  {(_id) => (
                    <>
                      <KeyList
                        view={m.view()}
                        onView={m.changeView}
                        tree={
                          <Tree
                            conn={c()}
                            progress={m.progress()}
                            selected={m.selected()}
                            onSelect={m.select}
                            autoexpand={m.autoexpand()}
                            autosort={m.autosort()}
                            version={m.version()}
                            onSearch={m.setQuery}
                            onDeletePattern={m.setDeleteTarget}
                            onScanAll={scan.rescan}
                            onRefresh={m.refresh}
                          />
                        }
                        footer={<ScanFooter conn={c()} scan={scan} />}
                        conn={c()}
                        progress={m.progress()}
                        selected={m.selected()}
                        onSelect={m.select}
                        query={m.query()}
                        onQuery={m.setQuery}
                        version={m.version()}
                        onDeletePattern={m.setDeleteTarget}
                        onNewKey={m.openNewKey}
                      />
                      <Splitter />
                      <ValuePanel conn={c()} selected={m.selected()} onSelect={m.select} />
                    </>
                  )}
                </Show>
              </div>
              <Show when={m.consoleOpen()}>
                {/* a database or mode switch replaces the session: the console starts over */}
                <Show when={c().id} keyed>
                  {(_id) => <Console conn={c()} onClose={m.closeConsole} onRefresh={m.refresh} autorun={m.autoconsole()} onAutorunDone={m.autoconsoleDone} />}
                </Show>
              </Show>
              <footer class="statusbar">
                <span class="truncate" classList={{ "statusbar-key": m.hasSelection(), "dim-label": !m.hasSelection() }}>
                  {m.statusText()}
                </span>
                <Show when={m.selectedBinary()}>
                  <span class="badge mark" title="This name is not valid UTF-8; other bytes are shown as \xNN">
                    binary
                  </span>
                </Show>
                <span class="spacer" />
                <span class="statusbar-hint">
                  <kbd>Ctrl+K</kbd> jump to key
                </span>
                <button class="statusbar-hint statusbar-action" onClick={m.toggleConsole} title="Open or close the command console">
                  <kbd>Ctrl+`</kbd> console
                </button>
                <Show when={m.hasTree()}>
                  <span class="statusbar-hint">Right-click a tree node for its prefix</span>
                </Show>
              </footer>
              <Show when={m.palette()}>
                <JumpPalette scope={m.scope(c())} onJump={m.select} onClose={m.closePalette} />
              </Show>
              <Show when={m.deleteTarget()}>{(t) => <DeleteDialog conn={c()} target={t()} onClose={m.closeDelete} />}</Show>
              <Show when={m.flushOpen()}>
                <FlushDialog conn={c()} onClose={m.closeFlush} onDone={m.flushed} />
              </Show>
              <Show when={m.newKeyOpen()}>
                <NewKeyDialog conn={c()} onClose={m.closeNewKey} onCreated={m.created} />
              </Show>
            </div>
          );
        }}
      </Show>
      <Show when={m.prefsOpen()}>
        <Preferences onClose={m.closePrefs} />
      </Show>
      <DialogHost />
      <ToastHost />
    </>
  );
}
