// Connection manager (CONCEPT 3.4): the stored connections with their
// index size, an editor for one, and a quick connect by URL that stores
// nothing. Markup only; state and actions are in `connectionsModel.ts`.

import { For, Show } from "solid-js";
import { createConnectionsModel, createEditorModel, createQuickConnectModel, type ConnectionsProps, type EditorProps, type QuickConnectProps } from "./connectionsModel";
import Dialog from "./ui/Dialog";
import Icon from "./ui/Icon";
import Popover, { MenuItem } from "./ui/Popover";
import Select from "./ui/Select";

export default function Connections(props: ConnectionsProps) {
  const m = createConnectionsModel(props);
  return (
    <div class="app">
      <header class="headerbar">
        <button class="button small suggested-action" onClick={m.create}>
          <Icon name="plus" />
          New connection
        </button>
        <span class="spacer" />
        <button class="button flat image-button" onClick={props.onPreferences} title="Preferences" aria-label="Preferences">
          <Icon name="gear" />
        </button>
        <span class="headerbar-title">Connections</span>
      </header>
      <div class="connections">
        <div class="clamp">
          <Show when={props.error}>
            <p class="connections-error" role="alert">
              <Icon name="alert-circle" />
              <span>{props.error}</span>
            </p>
          </Show>
          <Show when={m.listError()}>
            <p class="connections-error" role="alert">
              <Icon name="alert-circle" />
              <span>{m.listError()}</span>
            </p>
          </Show>
          <Show
            when={m.profiles().length > 0}
            fallback={
              <div class="status-page compact">
                <Icon name="server" size={64} class="status-page-icon" />
                <p class="status-page-title">No connections yet</p>
                <p class="status-page-description">Add a Redis or Valkey server to browse its keys, or connect once by URL below.</p>
                <button class="button suggested-action" onClick={m.create}>
                  New connection
                </button>
              </div>
            }
          >
            <p class="list-group-title">Stored connections</p>
            <div class="boxed-list">
              <For each={m.profiles()}>
                {(p) => (
                  <div class="row" onDblClick={() => m.connect(p)}>
                    <div class="row-content">
                      <span class="title">
                        <strong>{p.name}</strong>
                        <span class={`badge env-${p.tag}`}>{p.tag}</span>
                      </span>
                      <span class="subtitle monospace">{m.where(p)}</span>
                    </div>
                    <div class="suffix">
                      <span class="caption dim-label numeric" title="Size of this connection's index on disk">
                        {m.indexLabel(p)}
                      </span>
                      <button class="button small" onClick={() => m.connect(p)} disabled={m.locked()}>
                        {m.connecting(p) ? "Connecting…" : "Connect"}
                      </button>
                      <Popover
                        align="end"
                        trigger={(ctl) => (
                          <button
                            class="button flat image-button small"
                            classList={{ active: ctl.open() }}
                            onClick={ctl.toggle}
                            disabled={m.locked()}
                            title={`More for ${p.name}`}
                            aria-label={`More for ${p.name}`}
                          >
                            <Icon name="more" />
                          </button>
                        )}
                      >
                        {(close) => (
                          <div role="menu">
                            <MenuItem onClick={() => (close(), m.edit(p))}>Edit…</MenuItem>
                            <MenuItem disabled={!p.indexBytes} onClick={() => (close(), m.clearIndex(p))} title="Delete the index on disk; the next connect rescans">
                              Clear index…
                            </MenuItem>
                            <div class="menu-separator" />
                            <MenuItem danger onClick={() => (close(), m.remove(p))}>
                              Delete…
                            </MenuItem>
                          </div>
                        )}
                      </Popover>
                    </div>
                  </div>
                )}
              </For>
            </div>
          </Show>
          <p class="list-group-title">Connect by URL</p>
          <p class="list-group-description">Opens a connection once. Nothing is stored.</p>
          <QuickConnect onConnect={props.onConnect} />
        </div>
      </div>
      <Show when={m.editing()}>
        {(d) => <Editor draft={d()} onClose={m.closeEditor} onSaved={m.saved} />}
      </Show>
    </div>
  );
}

function Editor(props: EditorProps) {
  const m = createEditorModel(props);
  return (
    <Dialog
      title={m.title()}
      size="lg"
      persistent
      onClose={props.onClose}
      onEnter={m.save}
      footer={
        <>
          <button class="btn" type="button" onClick={props.onClose} disabled={m.busy()}>
            Cancel
          </button>
          <button class="btn" type="button" onClick={m.save} disabled={m.busy()}>
            Save
          </button>
          <button class="btn btn-primary" type="button" onClick={m.saveAndConnect} disabled={m.busy()}>
            Save and connect
          </button>
        </>
      }
    >
      <div class="form-stack">
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Fill from a URL</span>
            <input class="entry monospace" value={m.url()} onInput={(e) => m.setUrl(e.currentTarget.value)} placeholder="redis://user:password@host:6379/0" title="redis://, rediss:// and valkey:// URLs" spellcheck={false} />
          </label>
          <button class="button" type="button" onClick={m.fill} disabled={!m.canFill()}>
            Fill
          </button>
        </div>
        <label class="form-group">
          <span class="form-label">Name</span>
          <input class="entry" ref={m.focusLater} value={m.d().name} onInput={(e) => m.setName(e.currentTarget.value)} spellcheck={false} />
        </label>
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Host</span>
            <input class="entry monospace" value={m.d().host} onInput={(e) => m.setHost(e.currentTarget.value)} spellcheck={false} />
          </label>
          <label class="form-group narrow">
            <span class="form-label">Port</span>
            <input class="entry monospace" type="number" min="1" max="65535" value={m.d().port} onInput={(e) => m.setPort(e.currentTarget.value)} />
          </label>
          <label class="form-group narrow" title="The database a connect opens; the header bar switches from there">
            <span class="form-label">Database</span>
            <input class="entry monospace" type="number" min="0" value={m.d().db} onInput={(e) => m.setDb(e.currentTarget.value)} />
          </label>
        </div>
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Username</span>
            <input class="entry" value={m.d().username ?? ""} onInput={(e) => m.setUsername(e.currentTarget.value)} placeholder="Default user" spellcheck={false} />
          </label>
          <label class="form-group grow">
            <span class="form-label">Password</span>
            <input
              class="entry"
              type="password"
              value={m.password()}
              onInput={(e) => m.setPassword(e.currentTarget.value)}
              placeholder={m.passwordHint()}
              autocomplete="off"
            />
          </label>
        </div>
        <Show when={m.canForget()}>
          <label class="check-row">
            <input type="checkbox" class="check" checked={m.forget()} onChange={(e) => m.setForget(e.currentTarget.checked)} /> Remove the stored password from the keychain
          </label>
        </Show>
        <div class="boxed-list">
          <label class="row">
            <div class="row-content">
              <span class="title">TLS</span>
            </div>
            <div class="suffix">
              <input type="checkbox" class="switch" role="switch" checked={m.d().tls} onChange={(e) => m.setTls(e.currentTarget.checked)} />
            </div>
          </label>
          {/* always present, so switching TLS does not resize the dialog */}
          <label class="row" classList={{ disabled: !m.d().tls }}>
            <div class="row-content">
              <span class="title">Skip certificate verification</span>
              <span class="subtitle">Only for servers with a certificate you cannot verify</span>
            </div>
            <div class="suffix">
              <input
                type="checkbox"
                class="switch"
                role="switch"
                checked={m.tlsInsecure()}
                disabled={!m.d().tls}
                onChange={(e) => m.setTlsInsecure(e.currentTarget.checked)}
              />
            </div>
          </label>
        </div>
        <div class="boxed-list">
          <label class="row">
            <div class="row-content">
              <span class="title">SSH tunnel</span>
              <span class="subtitle">Reach the server, and every node it announces, through a bastion host</span>
            </div>
            <div class="suffix">
              <input type="checkbox" class="switch" role="switch" checked={m.sshOn()} onChange={(e) => m.setSshOn(e.currentTarget.checked)} />
            </div>
          </label>
        </div>
        {/* always present, so switching the tunnel does not resize the dialog */}
        <fieldset class="form-stack ssh-fields" disabled={m.sshOff()}>
          <div class="form-row">
            <label class="form-group grow">
              <span class="form-label">SSH host</span>
              <input class="entry" value={m.ssh().host} onInput={(e) => m.setSshHost(e.currentTarget.value)} placeholder="bastion.example" spellcheck={false} />
            </label>
            <label class="form-group narrow">
              <span class="form-label">SSH port</span>
              <input class="entry" type="number" min="1" max="65535" value={m.ssh().port} onInput={(e) => m.setSshPort(e.currentTarget.value)} />
            </label>
            <label class="form-group grow">
              <span class="form-label">SSH user</span>
              <input class="entry" value={m.ssh().user} onInput={(e) => m.setSshUser(e.currentTarget.value)} spellcheck={false} />
            </label>
          </div>
          <div class="form-row">
            <label class="form-group grow">
              <span class="form-label">Log in with</span>
              <Select value={m.ssh().auth} onChange={(e) => m.setSshAuth(e.currentTarget.value)}>
                <option value="agent">The SSH agent's keys</option>
                <option value="key">A private key file</option>
                <option value="password">A password</option>
              </Select>
            </label>
            <label class="form-group grow" classList={{ disabled: !m.sshUsesKey() }}>
              <span class="form-label">Key file</span>
              <input class="entry monospace" value={m.ssh().keyPath ?? ""} onInput={(e) => m.setSshKeyPath(e.currentTarget.value)} placeholder="~/.ssh/id_ed25519" disabled={!m.sshUsesKey()} spellcheck={false} />
            </label>
            <label class="form-group grow" classList={{ disabled: !m.sshNeedsSecret() }}>
              <span class="form-label">{m.sshSecretLabel()}</span>
              <input class="entry" type="password" value={m.sshSecret()} onInput={(e) => m.setSshSecret(e.currentTarget.value)} placeholder={m.sshSecretHint()} disabled={!m.sshNeedsSecret()} autocomplete="off" />
            </label>
          </div>
          <Show when={m.canForgetSsh()}>
            <label class="check-row">
              <input type="checkbox" class="check" checked={m.forgetSsh()} onChange={(e) => m.setForgetSsh(e.currentTarget.checked)} /> Remove the stored SSH secret from the keychain
            </label>
          </Show>
        </fieldset>
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Environment</span>
            <Select value={m.d().tag} onChange={(e) => m.setTag(e.currentTarget.value)}>
              <option value="dev">dev</option>
              <option value="staging">staging</option>
              <option value="prod">prod (polite scan, ephemeral index, writes confirmed)</option>
            </Select>
          </label>
          <label class="form-group grow">
            <span class="form-label">Index storage</span>
            <Select value={m.ephemeral()} onChange={(e) => m.setEphemeral(e.currentTarget.value)}>
              <option value="">By environment (ephemeral on prod)</option>
              <option value="off">Kept on disk</option>
              <option value="on">Ephemeral: deleted on disconnect</option>
            </Select>
          </label>
        </div>
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Index</span>
            <Select value={m.indexMode()} onChange={(e) => m.setIndexMode(e.currentTarget.value)}>
              <option value="">By size (everything up to 20 million keys, on demand above)</option>
              <option value="full">Everything: one background scan, then exact counts and search</option>
              <option value="onDemand">On demand: a sample first, a subtree when it is opened</option>
              <option value="none">None: plain SCAN pages and jump to key, no tree</option>
            </Select>
          </label>
        </div>
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Scan speed</span>
            <Select value={m.scanProfile()} onChange={(e) => m.setScanProfile(e.currentTarget.value)}>
              <option value="">By environment (polite on prod, fast otherwise)</option>
              <option value="fast">Fast: continuous SCAN calls</option>
              <option value="normal">Normal: small gaps, backs off when the server gets busy</option>
              <option value="polite">Polite: replicas only, 50 ms gaps</option>
            </Select>
          </label>
          <label class="form-group grow">
            <span class="form-label">Scan from</span>
            <Select value={m.d().scanTarget} onChange={(e) => m.setScanTarget(e.currentTarget.value)}>
              <option value="auto">A replica when one is reachable, else the primary</option>
              <option value="primary">The primary (or primaries of a cluster)</option>
              <option value="replica">Replicas only</option>
            </Select>
          </label>
        </div>
        <Show when={m.error()}>
          <p class="error-text" role="alert">
            {m.error()}
          </p>
        </Show>
      </div>
    </Dialog>
  );
}

function QuickConnect(props: QuickConnectProps) {
  const m = createQuickConnectModel(props);
  return (
    <form class="quick-connect" onSubmit={m.submit}>
      <div class="boxed-list">
        <label class="row">
          <div class="row-content">
            <span class="title">Server URL</span>
          </div>
          <input class="entry monospace url" type="text" value={m.url()} onInput={(e) => m.setUrl(e.currentTarget.value)} spellcheck={false} />
        </label>
        <div class="row">
          <div class="row-content">
            <span class="title">Environment</span>
            <span class="subtitle">prod scans politely, keeps no index on disk and confirms every write</span>
          </div>
          <div class="suffix">
            <Select compact value={m.tag()} onChange={(e) => m.setTag(e.currentTarget.value)} aria-label="Environment">
              <option value="dev">dev</option>
              <option value="staging">staging</option>
              <option value="prod">prod</option>
            </Select>
          </div>
        </div>
        <div class="row">
          <div class="row-content">
            <span class="title">Index</span>
            <span class="subtitle">By size: everything up to 20 million keys, on demand above</span>
          </div>
          <div class="suffix">
            <Select compact value={m.indexMode()} onChange={(e) => m.setIndexMode(e.currentTarget.value)} aria-label="Index">
              <option value="">By size</option>
              <option value="full">Everything</option>
              <option value="onDemand">On demand: subtrees as they are opened</option>
              <option value="none">None: plain SCAN pages</option>
            </Select>
          </div>
        </div>
        <div class="row">
          <div class="row-content">
            <span class="title">Scan speed</span>
          </div>
          <div class="suffix">
            <Select compact value={m.profile()} onChange={(e) => m.setProfile(e.currentTarget.value)} aria-label="Scan speed">
              <option value="">By environment</option>
              <option value="fast">Fast: continuous SCAN calls</option>
              <option value="normal">Normal: small gaps, backs off when busy</option>
              <option value="polite">Polite: replicas only, 50 ms gaps</option>
            </Select>
          </div>
        </div>
        <div class="row">
          <div class="row-content">
            <span class="title">Scan from</span>
          </div>
          <div class="suffix">
            <Select compact value={m.target()} onChange={(e) => m.setTarget(e.currentTarget.value)} aria-label="Scan from">
              <option value="auto">A replica when reachable, else the primary</option>
              <option value="primary">The primary</option>
              <option value="replica">Replicas only</option>
            </Select>
          </div>
        </div>
      </div>
      <button class="button" type="submit" disabled={m.busy()}>
        {m.busy() ? "Connecting…" : "Connect"}
      </button>
    </form>
  );
}