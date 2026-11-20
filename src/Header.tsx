// The header bar: which connection and database are open, the live toggle
// for keyspace notifications (CONCEPT 7) and the primary menu. Server
// details sit in the connection popover; what the user must know about the
// connection or its scan shows as a banner underneath. Markup only; state
// and actions are in `headerModel.ts`.

import { For, Show } from "solid-js";
import { createHeaderModel, type HeaderProps, type PropertyProps } from "./headerModel";
import Icon from "./ui/Icon";
import Popover, { MenuItem } from "./ui/Popover";
import Select from "./ui/Select";

export default function Header(props: HeaderProps) {
  const m = createHeaderModel(props);
  return (
    <>
      <header class="headerbar" data-env={props.conn.tag}>
        <Popover
          class="conn-popover"
          trigger={(ctl) => (
            <button class="button flat conn-button" classList={{ active: ctl.open() }} onClick={ctl.toggle} title="Connection details" aria-haspopup="dialog" aria-expanded={ctl.open()}>
              <Icon name="server" />
              <span class="conn-name">{props.conn.label}</span>
              <span class={`badge env-${props.conn.tag}`} title={m.envTitle()}>
                {props.conn.tag}
              </span>
              <Icon name="chevron-down" size={12} />
            </button>
          )}
        >
          {() => (
            <div class="boxed-list">
              <Property name="Server" value={m.serverLine()} />
              <Property name="Layout" value={m.layoutLine()} />
              <Show when={m.hasNodes()}>
                <div class="row property">
                  <div class="row-content">
                    <span class="title">Nodes</span>
                    <span class="node-list">{m.nodeList()}</span>
                  </div>
                </div>
              </Show>
              <Property name="Keys" value={m.keysLine()} />
              <Show when={m.hasServerMemory()}>
                <Property name="Server memory" value={m.serverMemory()} />
              </Show>
              <Show when={m.mem()}>{(s) => <Property name="Keyfold memory" value={m.memLine(s())} title={m.memTitle(s())} />}</Show>
            </div>
          )}
        </Popover>
        <Select compact value={props.conn.db} disabled={m.dbDisabled()} onFocus={m.loadDbs} onChange={(e) => m.switchDb(e.currentTarget)} aria-label="Database" title={m.dbTitle()}>
          <For each={m.dbOptions()}>
            {(o) => (
              <option value={o.db} selected={m.isOpenDb(o.db)}>
                db {o.db}, {m.keysLabel(o.keys)}
              </option>
            )}
          </For>
        </Select>
        <span class="spacer" />
        <button
          class="button flat console-button"
          classList={{ active: props.consoleOpen }}
          onClick={props.onConsole}
          title="Run a command (Ctrl+`)"
          aria-pressed={props.consoleOpen}
        >
          <Icon name="terminal" />
          <span>Console</span>
        </button>
        <Show when={m.hasIndex()}>
          <label class="live-toggle" title={m.liveTitle()}>
            <span classList={{ "success-text": m.live(), "dim-label": !m.live() }}>{m.liveLabel()}</span>
            <input type="checkbox" class="switch" role="switch" checked={m.liveWanted()} disabled={m.liveBusy()} onChange={(e) => m.toggleLive(e.currentTarget)} />
          </label>
        </Show>
        <Popover
          align="end"
          trigger={(ctl) => (
            <button class="button flat image-button" classList={{ active: ctl.open() }} onClick={ctl.toggle} title="Main menu" aria-label="Main menu" aria-haspopup="menu" aria-expanded={ctl.open()}>
              <Icon name="menu" />
            </button>
          )}
        >
          {(close) => (
            <div role="menu">
              <MenuItem onClick={() => (close(), props.onNewKey())}>New key…</MenuItem>
              <MenuItem onClick={() => (close(), props.onJump())} accel="Ctrl+K">
                Jump to key…
              </MenuItem>
              <MenuItem onClick={() => (close(), props.onConsole())} accel="Ctrl+`">
                Console
              </MenuItem>
              <MenuItem onClick={() => (close(), props.onPreferences())}>Preferences…</MenuItem>
              <div class="menu-separator" />
              <MenuItem danger disabled={props.scan.active()} onClick={() => (close(), props.onFlush())} title="FLUSHDB or FLUSHALL after typing the connection name">
                Flush database…
              </MenuItem>
              <MenuItem onClick={() => (close(), props.onDisconnect())}>Disconnect</MenuItem>
            </div>
          )}
        </Popover>
      </header>
      <Show when={m.showBanner()}>
        <div class="banner" role="status">
          <Icon name="warning" />
          <div class="banner-text">
            <For each={m.bannerLines()}>{(line) => <span>{line}</span>}</For>
          </div>
          <Show when={m.offersPrimary()}>
            <button class="button small" onClick={props.scan.scanPrimary}>
              Scan the primary anyway
            </button>
          </Show>
        </div>
      </Show>
    </>
  );
}

function Property(props: PropertyProps) {
  return (
    <div class="row property" title={props.title}>
      <div class="row-content">
        <span class="title">{props.name}</span>
        <span class="subtitle">{props.value}</span>
      </div>
    </div>
  );
}
