// Scan state shared by the sidebar footer and the banner: the index mode
// (CONCEPT 7), the phase of the running scan, the staleness line (CONCEPT
// 5.5), the nodes being read, and the gentleness profile and replica
// preference for the next scan (CONCEPT 4).

import { createSignal } from "solid-js";
import { api, fmtCount, type Connection, type IndexMode, type ScanProfile, type ScanProgress, type ScanTarget } from "./api";
import { ask } from "./dialogModel";

export type ScanModel = ReturnType<typeof createScanModel>;

export function createScanModel(
  conn: () => Connection,
  progress: () => ScanProgress | null,
  onRefresh: () => void,
  onMode: (mode: IndexMode | null) => Promise<void>,
) {
  const [scanError, setScanError] = createSignal<string | null>(null);
  const [modeBusy, setModeBusy] = createSignal(false);

  const mode = () => conn().indexMode;
  const noIndex = () => mode() === "none";
  const onDemand = () => mode() === "onDemand";
  const phase = () => progress()?.phase ?? (conn().scanning ? "starting" : null);
  const active = () => phase() === "scanning" || phase() === "paused" || phase() === "starting" || phase() === "finishing";
  const fraction = () => progress()?.fraction ?? null;
  const live = () => conn().live.phase === "live";
  const queue = () => progress()?.scanQueue ?? conn().scanQueue;
  const queued = () => (queue() > 0 ? `, ${queue()} queued` : "");
  /// What the scan in `progress` covers, in words.
  const what = (p: ScanProgress | null) => (p?.job === "subtree" && p.subtree ? `${p.subtree.text}*` : null);

  /// How fresh the index is, in words.
  const status = () => {
    if (noIndex()) return "No index: keys are read page by page";
    const p = progress();
    const ph = phase();
    if (ph === "paused") return `Scan paused${queued()}`;
    if (ph === "scanning" || ph === "starting") {
      const f = fraction();
      if (p?.job === "sample") return "Sampling keys";
      const sub = what(p);
      if (sub) {
        // a subtree scan only sees its matches: the share walked is an estimate
        return `Scanning ${sub}, ${f === null ? `${fmtCount(p?.scanned ?? 0)} keys` : `about ${(f * 100).toFixed(0)} % walked`}${queued()}`;
      }
      const idx = conn().index;
      const stale = idx.fullAt ? `Stale (scanned ${ago(idx.fullAt)}), refreshing` : "Scanning";
      return f === null ? `${stale}, ${fmtCount(p?.scanned ?? 0)} keys` : `${stale} ${(f * 100).toFixed(1)}%`;
    }
    if (ph === "finishing") return "Finishing scan";
    if (ph === "failed") return `Scan failed: ${p?.error ?? ""}`;
    if (ph === "stopped") return "Scan stopped, resumable";
    const liveTag = live() ? "Live, " : "";
    const scanned = live() ? "scanned" : "Scanned";
    if (ph === "complete" && p) {
      const took = `${(p.elapsedMs / 1000).toFixed(1)} s, ${p.calls} SCAN calls`;
      if (p.job === "sample") return `${liveTag}${live() ? "sampled" : "Sampled"} ${fmtCount(p.scanned)} keys (${took})`;
      const sub = what(p);
      if (sub) return `${liveTag}${scanned} ${sub} just now (${took})`;
      return `${liveTag}${scanned} just now (${took})`;
    }
    const idx = conn().index;
    if (idx.fullAt) return `${liveTag}${scanned} ${ago(idx.fullAt)}`;
    if (idx.status === "running") return "Interrupted scan, resumable";
    if (onDemand()) return `${liveTag}${live() ? "on" : "On"} demand: ${idx.subtrees === 1 ? "1 subtree" : `${idx.subtrees} subtrees`} scanned`;
    return "Not indexed";
  };

  /// Counters of the running scan.
  const stats = () => {
    const p = progress();
    if (!p || !active()) return "";
    const of = p.job === "full" && p.dbsize ? ` of ${fmtCount(p.dbsize)}` : "";
    return `${fmtCount(p.scanned)}${of}, COUNT ${p.count}, rtt ${p.lastRttMs.toFixed(1)} ms, ${fmtCount(p.indexKeys)} in index${
      p.backoff ? `, backing off (level ${p.backoff})` : ""
    }`;
  };

  /// Which nodes the running scan reads from, with each one's share.
  const nodesLine = () => {
    const p = progress();
    if (!p || !active() || p.nodes.length === 0) return "";
    const parts = p.nodes.map((n) => {
      const pct = p.job === "full" && n.dbsize ? ` ${Math.min(100, (n.scanned / n.dbsize) * 100).toFixed(0)}%` : "";
      return `${n.label} (${n.role}${pct}${n.done ? ", done" : ""}${n.backoff ? `, backoff ${n.backoff}` : ""})`;
    });
    const roles = new Set(p.nodes.map((n) => n.role));
    const what = p.nodes.length === 1 ? `the ${p.nodes[0].role}` : roles.size === 1 ? `${p.nodes.length} ${[...roles][0]}s` : `${p.nodes.length} nodes`;
    return `Reading ${what}: ${parts.join(", ")}`;
  };

  const setOptions = async (profile: ScanProfile, target: ScanTarget) => {
    try {
      await api.scanOptions(conn().id, profile, target);
      setScanError(null);
      onRefresh();
    } catch (e) {
      setScanError(String(e));
    }
  };

  /// Scan everything: the full index's rescan (or resume), and on demand
  /// the one action that walks and indexes the whole database, which the
  /// user confirms knowing its size.
  const rescan = async () => {
    setScanError(null);
    const c = conn();
    try {
      if (c.indexMode === "onDemand") {
        const ok = await ask({
          title: "Scan everything?",
          body: `${c.dbsize === null ? "The database" : `The database holds ${fmtCount(c.dbsize)} keys and`} was left to be indexed on demand. A full scan reads every key name into the index${
            c.index.ephemeral ? "" : " on disk"
          }; it can be paused, and it runs with the ${c.scanProfile} profile.`,
          confirm: "Scan everything",
          danger: c.tag === "prod",
        });
        if (!ok) return;
      }
      await api.scanStart(c.id, true);
    } catch (e) {
      setScanError(String(e));
    }
    onRefresh();
  };

  const setMode = async (value: string) => {
    setModeBusy(true);
    setScanError(null);
    try {
      await onMode((value || null) as IndexMode | null);
    } catch (e) {
      setScanError(String(e));
    } finally {
      setModeBusy(false);
    }
  };

  /// Notes of the scan that ran (not the reason one failed to start).
  const notes = () => conn().scanNotes.join(". ");
  const offersPrimary = () => /primary target|choose the primary/.test(scanError() ?? notes());
  const nodeErrors = () => (progress()?.nodes ?? []).filter((n) => n.error).map((n) => `${n.label}: ${n.error}`);
  const resumable = () => conn().index.status === "running";

  return {
    phase,
    active,
    fraction,
    live,
    status,
    stats,
    nodesLine,
    notes,
    scanError,
    offersPrimary,
    nodeErrors,
    setOptions,
    rescan,
    pause: () => api.scanPause(conn().id),
    resume: () => api.scanResume(conn().id),
    scanPrimary: () => setOptions(conn().scanProfile, "primary").then(rescan),

    // what the footer shows
    hasIndex: () => !noIndex(),
    canPause: () => phase() === "scanning" || phase() === "starting",
    isPaused: () => phase() === "paused",
    /// A subtree scan, or scans waiting behind the running one, can be stopped.
    canStop: () => active() && (progress()?.job === "subtree" || queue() > 0),
    stop: () => api.scanCancel(conn().id).then(onRefresh),
    canRescan: () => !noIndex() && !active(),
    rescanLabel: () => (resumable() ? "Resume scan" : onDemand() ? "Scan everything" : "Rescan"),
    rescanIcon: (): "play" | "refresh" => (resumable() ? "play" : "refresh"),
    barWidth: () => `${(fraction() ?? 0.02) * 100}%`,
    barNow: () => Math.round((fraction() ?? 0) * 100),

    // options
    modeValue: () => (conn().indexModeAuto ? "" : mode()),
    modeBusy,
    setMode: (value: string) => void setMode(value),
    bySizeLabel: () => `By size (${mode() === "full" ? "everything" : mode() === "onDemand" ? "on demand" : "none"})`,
    setProfile: (value: string) => void setOptions(value as ScanProfile, conn().scanTarget),
    setTarget: (value: string) => void setOptions(conn().scanProfile, value as ScanTarget),
    optionsNote: () => (noIndex() ? "Speed and source apply to the next page read." : "Speed and source apply to the next scan. A change of the index reconnects."),
  };
}

export function ago(unix: number): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix));
  if (s < 60) return `${s} s ago`;
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} days ago`;
}
