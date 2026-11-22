// State and actions of the header bar: the database selector, the live
// toggle for keyspace notifications (CONCEPT 7), the memory figures of the
// connection popover and the banner lines. The markup is in `Header.tsx`.

import { createEffect, createSignal, on, onCleanup, onMount } from "solid-js";
import { api, fmtBytes, fmtCount, type Connection, type Databases, type MemoryStats } from "./api";
import { ask } from "./dialogModel";
import type { ScanModel } from "./scanModel";

export interface HeaderProps {
  conn: Connection;
  scan: ScanModel;
  onDisconnect: () => void;
  /// Switch to another logical database; rejects when the server refuses it.
  onSwitchDb: (db: number) => Promise<void>;
  onFlush: () => void;
  onRefresh: () => void;
  onNewKey: () => void;
  onJump: () => void;
  onConsole: () => void;
  /// The console panel is showing.
  consoleOpen: boolean;
  onPreferences: () => void;
}

export interface PropertyProps {
  name: string;
  value: string;
  title?: string;
}

export function createHeaderModel(props: HeaderProps) {
  const [mem, setMem] = createSignal<MemoryStats | null>(null);
  const [liveBusy, setLiveBusy] = createSignal(false);

  const [dbs, setDbs] = createSignal<Databases | null>(null);
  const [dbBusy, setDbBusy] = createSignal(false);

  /// Key counts per database, read on connect, on every switch and when
  /// the selector is opened (other clients write to other databases).
  const loadDbs = () => {
    const id = props.conn.id;
    api
      .databases(id)
      .then((d) => {
        if (props.conn.id === id) setDbs(d);
      })
      .catch(() => {});
  };
  createEffect(on(() => props.conn.id, loadDbs));

  const dbOptions = () => {
    const d = dbs();
    const count = Math.max(d?.count ?? 1, props.conn.db + 1);
    const keys = new Map((d?.used ?? []).map((u) => [u.db, u.keys]));
    // the open database's count is the header's own, fresher DBSIZE
    if (props.conn.dbsize !== null) keys.set(props.conn.db, props.conn.dbsize);
    return Array.from({ length: count }, (_, db) => ({ db, keys: keys.get(db) ?? 0 }));
  };

  const switchDb = async (el: HTMLSelectElement) => {
    const db = Number(el.value);
    if (db === props.conn.db) return;
    setDbBusy(true);
    try {
      await props.onSwitchDb(db);
    } catch (e) {
      el.value = String(props.conn.db);
      await ask({ title: `Database ${db}`, body: String(e), confirm: "OK" });
    } finally {
      setDbBusy(false);
    }
  };

  onMount(() => {
    const tick = () => api.memoryStats().then(setMem).catch(() => {});
    tick();
    const t = setInterval(tick, 2000);
    onCleanup(() => clearInterval(t));
  });

  const layout = () => {
    const c = props.conn;
    if (c.cluster) return `Cluster, ${c.cluster.shards} shards, ${c.cluster.nodes} nodes${c.cluster.unassignedSlots ? `, ${c.cluster.unassignedSlots} slots unassigned` : ""}`;
    const replicas = c.nodes.filter((n) => n.role === "replica");
    const me = c.role === "slave" ? "replica" : c.role;
    return replicas.length ? `${c.mode}, ${me} with ${replicas.length} replica${replicas.length > 1 ? "s" : ""}` : `${c.mode}, ${me}`;
  };

  const banner = () => {
    const lines: string[] = [];
    if (props.conn.note) lines.push(props.conn.note);
    if (props.conn.indexStale) lines.push("A console command may have changed keys the index has not seen. Rescan to bring the index up to date.");
    const err = props.scan.scanError();
    if (err) lines.push(err);
    lines.push(...props.scan.nodeErrors());
    return lines;
  };
  const offersPrimary = () => props.scan.offersPrimary() && !props.scan.active();

  const live = () => props.conn.live.phase === "live";
  const liveWanted = () => props.conn.live.phase !== "off" && props.conn.live.phase !== "stopped";

  const toggleLive = async (el: HTMLInputElement) => {
    setLiveBusy(true);
    try {
      if (props.conn.live.phase !== "off" && props.conn.live.phase !== "stopped") {
        await api.liveSet(props.conn.id, false);
      } else {
        try {
          await api.liveSet(props.conn.id, true);
        } catch (e) {
          const msg = String(e);
          if (!msg.includes("notifications-off")) throw e;
          const ok = await ask({
            title: "Keyspace notifications are off on the server",
            body: `Live updates need notify-keyspace-events to publish key events. Turn them on with CONFIG SET on ${
              props.conn.cluster ? "every primary" : "the server"
            }? This is a server-wide setting that costs some CPU per write; it stays on until an operator changes it.`,
            confirm: "Enable on the server",
            danger: props.conn.tag === "prod",
          });
          if (!ok) return;
          await api.notifyEnable(props.conn.id, true);
          await api.liveSet(props.conn.id, true);
        }
      }
    } catch (e) {
      api.log("error", `live ${e}`);
      await ask({ title: "Live updates", body: String(e), confirm: "OK" });
    } finally {
      setLiveBusy(false);
      // the switch shows the server's answer, not the click
      el.checked = liveWanted();
      props.onRefresh();
    }
  };

  const liveLabel = () => {
    const l = props.conn.live;
    switch (l.phase) {
      case "live":
        return `Live, ${fmtCount(l.events)} events`;
      case "connecting":
        return "Connecting…";
      case "reconnecting":
        return `Reconnecting (${l.nodesLive}/${l.nodes})`;
      default:
        return "Live";
    }
  };

  return {
    mem,
    dbOptions,
    loadDbs,
    live,
    /// Live updates keep an index current; without one there is nothing to keep.
    hasIndex: () => props.conn.indexMode !== "none",
    liveWanted,
    liveBusy,
    liveLabel,
    offersPrimary,
    switchDb: (el: HTMLSelectElement) => void switchDb(el),
    toggleLive: (el: HTMLInputElement) => void toggleLive(el),
    envTitle: () => (props.conn.tag === "prod" ? "Polite scan, ephemeral index, every write confirmed" : undefined),
    serverLine: () => `${props.conn.server} ${props.conn.version}`,
    layoutLine: () => `${layout()}, RESP${props.conn.protocol}`,
    hasNodes: () => props.conn.nodes.length > 1,
    nodeList: () => props.conn.nodes.map((n) => `${n.addr} ${n.role}${n.online ? "" : " (offline)"}`).join("\n"),
    keysLine: () => (props.conn.dbsize === null ? "DBSIZE denied" : fmtCount(props.conn.dbsize)),
    hasDenied: () => props.conn.denied.length > 0,
    deniedLine: () => props.conn.denied.map((c) => c.replace("|", " ").toUpperCase()).join(", "),
    hasServerMemory: () => props.conn.usedMemory !== null,
    serverMemory: () => fmtBytes(props.conn.usedMemory),
    memLine: (s: MemoryStats) => `Backend ${fmtBytes(s.backendRss)}, webview ${fmtBytes(s.webviewRss)}`,
    memTitle: (s: MemoryStats) => s.processes.join(", "),
    dbDisabled: () => dbBusy() || dbOptions().length < 2,
    dbTitle: () =>
      dbs()?.count === 1
        ? "This server has one logical database"
        : `Logical database (SELECT)${dbs() && !dbs()!.configured ? "; the number of databases could not be read, 16 assumed" : ""}. Each database has its own index.`,
    isOpenDb: (db: number) => db === props.conn.db,
    keysLabel: (keys: number) => (keys ? `${fmtCount(keys)} keys` : "empty"),
    liveTitle: () =>
      props.conn.notify?.enabled === false
        ? `Keyspace notifications are off on the server (notify-keyspace-events="${props.conn.notify.value ?? ""}")`
        : "Keep the index live from keyspace notifications (opt-in; costs server CPU)",
    showBanner: () => banner().length > 0 || offersPrimary(),
    /// What the banner says: the warnings, or the scan notes behind the offer.
    bannerLines: () => (banner().length ? banner() : [props.scan.notes()]),
  };
}
