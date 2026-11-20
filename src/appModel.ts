// State and actions of the application shell: the open connection, the
// selection, the search query, which dialogs are open, and the startup
// automation the smoke tests drive. The markup is in `App.tsx`.

import { createSignal, onCleanup, onMount } from "solid-js";
import { api, type Connection, type DeleteTarget, type HostKey, type IndexMode, type KeyRef, type SshSettings, type ScanProfile, type ScanProgress, type ScanTarget } from "./api";
import { ask } from "./dialogModel";
import { isBinary, textKey } from "./keyRef";
import { pushRecent } from "./jumpPaletteModel";
import type { NavView } from "./keyListModel";
import { createScanModel } from "./scanModel";

const VIEW_KEY = "keyfold.navView";

function storedView(): NavView {
  try {
    return localStorage.getItem(VIEW_KEY) === "list" ? "list" : "tree";
  } catch {
    return "tree";
  }
}

export function createAppModel() {
  const [conn, setConn] = createSignal<Connection | null>(null);
  const [progress, setProgress] = createSignal<ScanProgress | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [selected, setSelected] = createSignal<KeyRef | null>(null);
  const [autoexpand, setAutoexpand] = createSignal<string | null>(null);
  const [query, setQuery] = createSignal("");
  const [view, setView] = createSignal<NavView>(storedView());
  const changeView = (next: NavView) => {
    setView(next);
    try {
      localStorage.setItem(VIEW_KEY, next);
    } catch {
      // the choice then lasts for this run only
    }
  };
  const [palette, setPalette] = createSignal(false);
  /// Bumped on every `index:changed` event: the panes refetch what they show.
  const [version, setVersion] = createSignal(0);
  const [deleteTarget, setDeleteTarget] = createSignal<DeleteTarget | null>(null);
  const [flushOpen, setFlushOpen] = createSignal(false);
  const [newKeyOpen, setNewKeyOpen] = createSignal(false);
  const [prefsOpen, setPrefsOpen] = createSignal(false);
  const [consoleOpen, setConsoleOpen] = createSignal(false);
  const [autoconsole, setAutoconsole] = createSignal<string[]>([]);

  /// Selecting a key anywhere makes it a recent one for Ctrl+K.
  const select = (key: KeyRef | null) => {
    setSelected(key);
    const c = conn();
    if (key && c) pushRecent(`${c.label} db${c.db}`, key);
  };

  /// Header numbers (DBSIZE, live status, scan notes) after a change.
  const refreshConn = async () => {
    const c = conn();
    if (!c) return;
    const fresh = await api.connectionInfo(c.id).catch(() => null);
    if (fresh && conn()?.id === c.id) setConn(fresh);
  };
  let lastLiveRefresh = 0;

  onMount(async () => {
    const unlisten = await api.onScanProgress((p) => {
      const c = conn();
      if (c && p.session === c.id) setProgress(p);
    });
    onCleanup(unlisten);
    const unlistenChanged = await api.onIndexChanged((p) => {
      const c = conn();
      if (!c || p.session !== c.id) return;
      setVersion((v) => v + 1);
      if (p.reason === "live") {
        // notifications can arrive four times a second; the header
        // numbers follow at most every two seconds
        const now = Date.now();
        if (now - lastLiveRefresh > 2000) {
          lastLiveRefresh = now;
          void refreshConn();
        }
      } else if (p.reason !== "mutate") {
        void refreshConn();
      }
    });
    onCleanup(unlistenChanged);
    const unlistenLive = await api.onLiveStatus((p) => {
      const c = conn();
      if (!c || p.session !== c.id) return;
      setConn({ ...c, live: p });
    });
    onCleanup(unlistenLive);
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        if (conn()) setPalette((p) => !p);
      }
      // the console, as terminals open theirs
      if ((e.ctrlKey || e.metaKey) && e.key === "`") {
        e.preventDefault();
        if (conn()) setConsoleOpen((o) => !o);
      }
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
    const opts = await api.startupOptions().catch(() => null);
    setAutoexpand(opts?.autoexpand ?? null);
    if (opts?.autoprofile) {
      // smoke test of the connection manager: store, list, connect, delete
      try {
        const u = await api.profileFromUrl(opts.autoprofile);
        // "user@host:port,keyfile": the stored connection goes through that bastion
        const [bastion, keyPath] = (opts.autoprofileSsh ?? "").split(",");
        const m = /^(.+)@(.+?)(?::(\d+))?$/.exec(bastion);
        const ssh: SshSettings | null = m ? { user: m[1], host: m[2], port: Number(m[3] ?? 22), auth: keyPath ? "key" : "agent", keyPath: keyPath || null } : null;
        autoTrust = ssh !== null;
        const saved = await api.profileSave(
          { id: null, name: "smoke test", host: u.host, port: u.port, db: u.db, username: u.username, tls: u.tls, tlsInsecure: u.tlsInsecure, tag: "dev", scanProfile: null, scanTarget: "auto", ephemeralIndex: null, indexMode: null, ssh },
          u.password,
        );
        api.log("info", `autoprofile saved ${JSON.stringify(saved)}`);
        await connectProfile(saved.id);
        const c = conn();
        api.log("info", `autoprofile connected ${c?.label} db ${c?.db} ${c?.dbsize} keys, index ${c?.index.path}${error() ? `, error ${error()}` : ""}`);
        for (let i = 0; i < 120 && c && progress()?.phase !== "complete"; i++) await new Promise((r) => setTimeout(r, 250));
        if (c) {
          await api.scanOptions(c.id, "normal", "primary");
          await disconnect();
        }
        api.log("info", `autoprofile list ${JSON.stringify(await api.profilesList())}`);
        await api.profileDelete(saved.id);
        api.log("info", `autoprofile deleted, ${(await api.profilesList()).length} left`);
      } catch (e) {
        api.log("error", `autoprofile ${e}`);
      }
    }
    if (opts?.autoconnect) {
      const [p, t] = (opts.autoscanopts ?? "").split(":");
      await connect(opts.autoconnect, opts.tag, (p as ScanProfile) || null, ((t as ScanTarget) || "auto") as ScanTarget, (opts.automode as IndexMode) || null);
      const c0 = conn();
      if (c0) api.log("info", `autoconnect layout ${JSON.stringify({ cluster: c0.cluster, nodes: c0.nodes, note: c0.note, scanProfile: c0.scanProfile, scanTarget: c0.scanTarget, scanNotes: c0.scanNotes, notify: c0.notify, indexMode: c0.indexMode })}`);
      if (opts.automode && c0) {
        // an index mode was forced: say what the connection holds once
        // its first scan and an automatic expand have had time to run
        void (async () => {
          for (let i = 0; i < 4; i++) {
            await new Promise((r) => setTimeout(r, 2500));
            await refreshConn();
            const c = conn();
            const pr = progress();
            api.log("info", `automode ${c?.indexMode}: index ${JSON.stringify(c?.index)}, queue ${c?.scanQueue}, last scan ${pr ? `${pr.job} ${pr.subtree?.text ?? ""} ${pr.phase} ${pr.scanned}` : "none"}`);
          }
        })();
      }
      for (const key of opts.autoselect ?? []) {
        await new Promise((r) => setTimeout(r, 2000));
        select(key);
      }
      const searches = async () => {
        for (const q of opts.autosearch ?? []) {
          await new Promise((r) => setTimeout(r, 2000));
          setQuery(q);
        }
      };
      await searches();
      if (opts.autofts) {
        const c = conn();
        if (c) {
          await new Promise((r) => setTimeout(r, 4000));
          const done = new Promise<void>((resolve) => {
            void api.onFtsProgress((p) => {
              api.log("info", `fts ${p.phase} ${p.done}/${p.total}${p.error ? ` ${p.error}` : ""}`);
              if (p.phase === "ready" || p.phase === "failed" || p.phase === "cancelled") resolve();
            });
          });
          setQuery("");
          await api.searchFts(c.id, true).catch((e) => api.log("error", `search_fts ${e}`));
          await done;
          await searches();
        }
      }
      if (opts.automutate || opts.autodelete || opts.autolive || opts.autoswitchdb !== null || opts.autoconsole.length > 0) {
        // wait for the scan to finish, so the writes land in the index directly
        for (let i = 0; i < 600; i++) {
          const ph = progress()?.phase;
          const c = conn();
          if (ph === "complete" || ph === "stopped" || ph === "failed" || (!ph && c && !c.scanning)) break;
          await new Promise((r) => setTimeout(r, 500));
        }
      }
      if (opts.autoswitchdb !== null) {
        const c = conn();
        if (c) {
          try {
            api.log("info", `autoswitchdb databases ${JSON.stringify(await api.databases(c.id))}`);
            await switchDb(opts.autoswitchdb);
            const n = conn();
            api.log("info", `autoswitchdb from ${c.label} (id ${c.id}) to ${n?.label} (id ${n?.id}, db ${n?.db}, ${n?.dbsize} keys, index ${n?.index.path})`);
            for (let i = 0; i < 120 && progress()?.phase !== "complete"; i++) await new Promise((r) => setTimeout(r, 250));
            await refreshConn();
            const root = await api.treeRoot(conn()!.id);
            api.log("info", `autoswitchdb scan ${progress()?.phase}, ${conn()?.index.keys} keys in index, root ${JSON.stringify(root).slice(0, 200)}`);
          } catch (e) {
            api.log("error", `autoswitchdb ${e}`);
          }
        }
      }
      if (opts.automutate) {
        const c = conn();
        const key = opts.automutate;
        if (c) {
          const log = (what: string, r: unknown) => api.log("info", `automutate ${what}: ${JSON.stringify(r)}`);
          try {
            log("set", await api.mutate(c.id, textKey(key), { op: "setString", value: "hello", keepTtl: false }, true));
            log("expire", await api.mutate(c.id, textKey(key), { op: "expire", seconds: 600 }, true));
            log("hset", await api.mutate(c.id, textKey(`${key}:hash`), { op: "hashSet", field: "f", value: "1" }, true));
            log("hdel", await api.mutate(c.id, textKey(`${key}:hash`), { op: "hashDel", field: "f" }, true));
            log("rename", await api.mutate(c.id, textKey(key), { op: "rename", to: `${key}:renamed` }, true));
            select(textKey(`${key}:renamed`));
            await new Promise((r) => setTimeout(r, 1500));
            log("delete", await api.mutate(c.id, textKey(`${key}:renamed`), { op: "delete" }, true));
          } catch (e) {
            api.log("error", `automutate ${e}`);
          }
        }
      }
      if (opts.autoconsole.length > 0 && conn()) {
        // the console runs them itself, so the panel shows what a user would see
        setAutoconsole(opts.autoconsole);
        setConsoleOpen(true);
      }
      if (opts.autolive) {
        const c = conn();
        if (c) {
          try {
            const p = progress();
            api.log("info", `autolive scan ${p?.phase} nodes ${JSON.stringify(p?.nodes.map((n) => [n.label, n.role, n.scanned, n.calls, n.backoff]))} notes ${JSON.stringify(p?.scanNotes)}`);
            const setting = await api.notifySetting(c.id);
            api.log("info", `autolive notify ${JSON.stringify(setting)}`);
            if (setting.enabled === false) {
              const v = await api.notifyEnable(c.id, true);
              api.log("info", `autolive enabled notify-keyspace-events=${v}`);
            }
            const st = await api.liveSet(c.id, true);
            api.log("info", `autolive status ${JSON.stringify(st)}`);
            // leave time for the smoke script to write keys from outside
            for (let i = 0; i < 16; i++) {
              await new Promise((r) => setTimeout(r, 500));
              const now = await api.liveStatus(c.id);
              if (i % 4 === 3) api.log("info", `autolive status ${JSON.stringify(now)}`);
            }
          } catch (e) {
            api.log("error", `autolive ${e}`);
          }
        }
      }
      if (opts.autodelete) {
        const c = conn();
        if (c) {
          try {
            const target = { query: opts.autodelete, prefix: null };
            const preview = await api.deletePreview(c.id, target);
            api.log("info", `autodelete preview ${JSON.stringify(preview)}`);
            const done = new Promise<void>((resolve) => {
              void api.onDeleteProgress((p) => {
                api.log("info", `autodelete ${p.phase} ${p.deleted}/${p.total} removed ${p.removed} calls ${p.calls}${p.error ? ` ${p.error}` : ""}`);
                if (p.phase === "complete" || p.phase === "cancelled" || p.phase === "failed") resolve();
              });
            });
            await api.deleteStart(c.id, target, true);
            await done;
            await refreshConn();
            api.log("info", `autodelete dbsize now ${conn()?.dbsize}`);
          } catch (e) {
            api.log("error", `autodelete ${e}`);
          }
        }
      }
    }
  });

  const connect = async (url: string, tag: string, profile: ScanProfile | null = null, target: ScanTarget = "auto", indexMode: IndexMode | null = null) => {
    setError(null);
    try {
      const c = await api.connect(url, tag, profile, target, indexMode);
      setProgress(null);
      setConn(c);
    } catch (e) {
      setError(String(e));
    }
  };

  /// A connect through an SSH bastion whose host key is not on record
  /// stops and reports the key; the user reads its fingerprint and decides.
  const HOST_KEY = "ssh-host-key-unknown ";
  /// Development aid: the smoke run of a tunnelled profile accepts the key itself.
  let autoTrust = false;
  const connectTrusting = async (id: string): Promise<Connection> => {
    try {
      return await api.connectProfile(id);
    } catch (e) {
      const text = String(e);
      const at = text.indexOf(HOST_KEY);
      if (at < 0) throw e;
      const key = JSON.parse(text.slice(at + HOST_KEY.length)) as HostKey;
      if (autoTrust) api.log("info", `autoprofile trusts the SSH host key of ${key.host}:${key.port}: ${key.fingerprint}`);
      const ok = autoTrust || await ask({
        title: "Unknown SSH host key",
        body: `The SSH server ${key.host}:${key.port} is not known yet. Its key has the fingerprint\n\n${key.fingerprint}\n\nCompare it with what the server's administrator gave you. Trust it only if they match.`,
        confirm: "Trust and connect",
      });
      if (!ok) throw new Error(`The SSH host key of ${key.host}:${key.port} was not accepted.`);
      await api.sshTrust(key);
      return api.connectProfile(id);
    }
  };

  const connectProfile = async (id: string) => {
    setError(null);
    try {
      const c = await connectTrusting(id);
      setProgress(null);
      setSelected(null);
      setQuery("");
      setConn(c);
    } catch (e) {
      setError(String(e));
    }
  };

  const disconnect = async () => {
    const c = conn();
    if (!c) return;
    setConn(null);
    setProgress(null);
    await api.disconnect(c.id).catch(() => {});
  };

  /// Another logical database of the same server. The backend answers
  /// with a new session, so every pane starts over; a refusal throws and
  /// leaves the open database as it was.
  const switchDb = async (db: number) => {
    const c = conn();
    if (!c) return;
    const next = await api.switchDb(c.id, db);
    if (conn()?.id !== c.id) return;
    setSelected(null);
    setQuery("");
    setDeleteTarget(null);
    setFlushOpen(false);
    setNewKeyOpen(false);
    setPalette(false);
    setProgress(null);
    setConn(next);
  };

  /// Another index mode for the open connection. Like a database switch
  /// the backend answers with a new session and every pane starts over;
  /// the index on disk stays. A refusal throws; when the old session is
  /// already closed by then, the connection is gone.
  const setIndexMode = async (mode: IndexMode | null) => {
    const c = conn();
    if (!c) return;
    try {
      const next = await api.setIndexMode(c.id, mode);
      if (conn()?.id !== c.id) return;
      setDeleteTarget(null);
      setProgress(null);
      setConn(next);
    } catch (e) {
      const alive = await api.connectionInfo(c.id).catch(() => null);
      if (!alive && conn()?.id === c.id) {
        setConn(null);
        setProgress(null);
        setError(String(e));
      }
      throw e;
    }
  };

  return {
    conn,
    progress,
    error,
    selected,
    autoexpand,
    query,
    view,
    palette,
    version,
    deleteTarget,
    flushOpen,
    newKeyOpen,
    prefsOpen,
    consoleOpen,
    autoconsole,
    autoconsoleDone: async () => {
      setAutoconsole([]);
      await refreshConn();
      api.log("info", `autoconsole done: ${conn()?.index.keys} keys in index, dbsize ${conn()?.dbsize}, stale ${conn()?.indexStale}`);
    },
    toggleConsole: () => setConsoleOpen((o) => !o),
    closeConsole: () => setConsoleOpen(false),
    changeView,
    select,
    setQuery,
    setDeleteTarget,
    connect,
    connectProfile,
    disconnect,
    switchDb,
    refresh: () => void refreshConn(),
    /// The scan state of one open connection, shared by header and footer.
    createScan: (c: () => Connection) => createScanModel(c, progress, () => void refreshConn(), setIndexMode),
    /// Under which name the jump palette keeps its recent keys.
    scope: (c: Connection) => `${c.label} db${c.db}`,
    hasSelection: () => !!selected(),
    hasTree: () => conn()?.indexMode !== "none",
    statusText: () => selected()?.text ?? "No key selected",
    selectedBinary: () => isBinary(selected()),
    openPalette: () => setPalette(true),
    closePalette: () => setPalette(false),
    closeDelete: () => setDeleteTarget(null),
    openFlush: () => setFlushOpen(true),
    closeFlush: () => setFlushOpen(false),
    flushed: () => {
      select(null);
      setQuery("");
    },
    openNewKey: () => setNewKeyOpen(true),
    closeNewKey: () => setNewKeyOpen(false),
    /// A new key's name was typed, so it is text.
    created: (key: string) => {
      setQuery("");
      select(textKey(key));
    },
    openPrefs: () => setPrefsOpen(true),
    closePrefs: () => setPrefsOpen(false),
  };
}
