// Typed wrappers around the backend command surface. Every call is windowed:
// the webview never asks for more than 200 rows at once.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const WINDOW = 200;

export interface IndexInfo {
  keys: number;
  status: "idle" | "running" | "complete";
  generation: number;
  scannedAt: number | null;
  /// When the whole keyspace was last scanned to the end.
  fullAt: number | null;
  /// Subtrees with a completed scan of their own.
  subtrees: number;
  ephemeral: boolean;
  path: string;
}

/// How much of the keyspace a connection indexes (CONCEPT 7): everything,
/// the subtrees that were opened, or nothing.
export type IndexMode = "full" | "onDemand" | "none";

export type ScanProfile = "fast" | "normal" | "polite";
export type ScanTarget = "auto" | "primary" | "replica";
export type LivePhase = "off" | "connecting" | "live" | "reconnecting" | "stopped";

export interface NodeInfo {
  addr: string;
  role: "primary" | "replica";
  online: boolean;
}

export interface ClusterInfo {
  shards: number;
  nodes: number;
  unassignedSlots: number;
}

export interface LiveStatus {
  session: number;
  phase: LivePhase;
  nodesLive: number;
  nodes: number;
  events: number;
  appliedAdded: number;
  appliedRemoved: number;
  error: string | null;
}

export interface NotifySetting {
  value: string | null;
  enabled: boolean | null;
}

export interface Connection {
  id: number;
  label: string;
  /// dev, staging or prod; prod confirms every write.
  tag: "dev" | "staging" | "prod";
  server: string;
  version: string;
  mode: string;
  role: string;
  protocol: number;
  db: number;
  /// Summed over the primaries of a cluster.
  dbsize: number | null;
  usedMemory: number | null;
  index: IndexInfo;
  indexMode: IndexMode;
  /// The mode was chosen by the database's size, not by the profile.
  indexModeAuto: boolean;
  scanning: boolean;
  /// Subtree scans waiting for the running one.
  scanQueue: number;
  /// A console command may have changed keys the index has not seen.
  indexStale: boolean;
  /// The server and its replicas, or every cluster node.
  nodes: NodeInfo[];
  cluster: ClusterInfo | null;
  note: string | null;
  scanProfile: ScanProfile;
  scanTarget: ScanTarget;
  scanNotes: string[];
  live: LiveStatus;
  notify: NotifySetting | null;
}

/// The SSH bastion a stored connection is reached through (CONCEPT 8).
/// Its password or key passphrase is never part of it.
export interface SshSettings {
  host: string;
  port: number;
  user: string;
  /// The running SSH agent, a private key file, or a password.
  auth: "agent" | "key" | "password";
  keyPath: string | null;
}

/// An SSH host key that is not on record yet, as a connect reports it.
export interface HostKey {
  host: string;
  port: number;
  fingerprint: string;
  openssh: string;
}

/// What the connection editor sends; `id: null` creates a profile.
export interface ProfileInput {
  id: string | null;
  name: string;
  host: string;
  port: number;
  /// The database a connect opens.
  db: number;
  username: string | null;
  tls: boolean;
  tlsInsecure: boolean;
  tag: "dev" | "staging" | "prod";
  /// `null` follows the tag.
  scanProfile: ScanProfile | null;
  scanTarget: ScanTarget;
  /// `null` follows the tag (ephemeral on prod).
  ephemeralIndex: boolean | null;
  /// `null` decides by the database's size.
  indexMode: IndexMode | null;
  /// `null`: the server is reached directly.
  ssh: SshSettings | null;
}

/// A stored connection (CONCEPT 3.4). The password is never part of it.
export interface StoredProfile extends ProfileInput {
  id: string;
  /// A password for it is in the OS keychain.
  passwordSaved: boolean;
  /// The bastion's password or key passphrase is in the OS keychain.
  sshSecretSaved: boolean;
  /// Size of its index folder on disk.
  indexBytes: number;
}

export interface UrlParts {
  name: string;
  host: string;
  port: number;
  db: number;
  username: string | null;
  password: string | null;
  tls: boolean;
  tlsInsecure: boolean;
}

/// The logical databases of the server, for the header's selector.
export interface Databases {
  current: number;
  /// Databases `0..count` can be selected.
  count: number;
  /// `count` comes from the server's configuration, not the default of 16.
  configured: boolean;
  /// The non-empty databases with their key counts.
  used: { db: number; keys: number }[];
}

export type ScanPhase = "starting" | "scanning" | "paused" | "finishing" | "complete" | "stopped" | "failed";

export interface ScanProgress {
  session: number;
  job: "full" | "sample" | "subtree";
  /// The prefix of a subtree scan.
  subtree: KeyRef | null;
  /// How far the scan is, 0 to 1, when that can be told.
  fraction: number | null;
  /// `fraction` is estimated from the walk, not counted.
  estimate: boolean;
  /// Subtree scans waiting for this one.
  scanQueue: number;
  phase: ScanPhase;
  generation: number;
  resumed: boolean;
  partial: boolean;
  dbsize: number | null;
  scanned: number;
  indexedNew: number;
  calls: number;
  count: number;
  lastRttMs: number;
  elapsedMs: number;
  queued: number;
  pruned: number;
  indexKeys: number;
  error: string | null;
  nodes: NodeProgress[];
  /// Highest backoff level over the nodes (normal profile).
  backoff: number;
  scanNotes: string[];
}

export interface NodeProgress {
  label: string;
  role: "primary" | "replica";
  dbsize: number | null;
  scanned: number;
  calls: number;
  count: number;
  done: boolean;
  backoff: number;
  error: string | null;
}

/// A key name: `text` alone when it is UTF-8, else an escaped text for
/// display plus the exact bytes as hex (see `keyRef.ts`).
export interface KeyRef {
  text: string;
  raw: string | null;
}

export interface Row {
  name: string;
  /// Exact bytes as hex when the name is not UTF-8.
  raw: string | null;
  keyType: string | null;
  ttl: number | null;
  mem: number | null;
}

export interface ListWindow {
  total: number;
  exact: boolean;
  rows: Row[];
}

export interface KeyMeta {
  exists: boolean;
  keyType: string | null;
  ttl: number | null;
  mem: number | null;
}

export interface Classes {
  uuid: number;
  ulid: number;
  hex: number;
  numeric: number;
  date: number;
  opaque: number;
  email: number;
  literal: number;
}

export interface TreeNode {
  segment: string;
  raw: string | null;
  prefix: string;
  prefixRaw: string | null;
  /// The key this node stands for, when it is one (`leaf`).
  key: KeyRef | null;
  /// When a completed scan last saw every key under this node (unix
  /// seconds); null when the index knows the subtree only in part.
  scannedAt: number | null;
  /// A scan that will make this subtree fresh runs or waits.
  scan: "idle" | "queued" | "running";
  leaf: boolean;
  count: number;
  segNum: number | null;
  children: number;
  classes: Classes;
}

export interface TreeAfter {
  segment: string;
  raw: string | null;
  count: number;
  segNum: number | null;
}

export interface TreeWindow {
  nodes: TreeNode[];
  exact: boolean;
}

export interface TreeRoot {
  children: number;
  classes: Classes;
  delimiter: string;
  keys: number;
  exact: boolean;
  /// When the whole keyspace was last scanned to the end.
  scannedAt: number | null;
}

export type TreeSort = "name" | "count" | "numeric";

export interface ChildFilter {
  segNumMin?: number;
  segNumMax?: number;
  segMin?: string;
  segMinRaw?: string | null;
  segMax?: string;
  segMaxRaw?: string | null;
  class?: string;
  excludeClasses?: string[];
}

export interface Bucket {
  label: string;
  segNumMin: number | null;
  segNumMax: number | null;
  segMin: string | null;
  segMinRaw: string | null;
  segMax: string | null;
  segMaxRaw: string | null;
  count: number;
}

export interface FoldEntry {
  segment: string;
  raw: string | null;
  parents: number;
  keys: number;
  leaves: number;
  coverage: number;
}

export interface Fold {
  class: string;
  label: string;
  members: number;
  share: number;
  range: [number, number] | null;
  samples: string[];
  summary: FoldEntry[];
  summaryDistinct: number;
  membersWithChildren: number;
  buckets: Bucket[];
}

export interface FoldInfo {
  distinct: number;
  evidence: string;
  folded: boolean;
  folds: Fold[];
  literals: TreeNode[];
  computedMs: number;
  cached: boolean;
}

export type FoldRule = "force" | "forbid" | "none";

export interface KeyInfo {
  exists: boolean;
  keyType: string;
  viewer: "string" | "hash" | "set" | "zset" | "list" | "stream" | "raw";
  ttl: number | null;
  mem: number | null;
  encoding: string | null;
  length: number | null;
  kind: "json" | "text" | "gzip" | "msgpack" | "binary" | "empty" | null;
  previewComplete: boolean;
  previewBytes: number;
}

export interface Entry {
  field: string;
  fieldRaw: string | null;
  value: string;
  valueRaw: string | null;
  score: number | null;
}

export interface ValuePage {
  entries: Entry[];
  cursor: number;
  next: string | null;
  done: boolean;
}

export type ValueCursor = { kind: "scan"; value: number } | { kind: "rank"; value: number } | { kind: "streamId"; value: string };

export interface DumpInfo {
  length: number;
  peekHex: string;
}

export type SearchMode = "glob" | "substring" | "regex";

/// What the console answered to one typed line (CONCEPT section 8).
export interface ConsoleReply {
  /// `refused`: not sent, the lines say why. `confirm`: not sent yet.
  status: "ok" | "error" | "refused" | "confirm";
  /// The reply in redis-cli style, already cut to a bound.
  lines: string[];
  truncated: boolean;
  /// The node that answered.
  node: string | null;
  tookMs: number;
  confirm: { level: "write" | "danger" | "typed"; text: string; expect: string | null } | null;
  /// What the command meant for the index.
  indexNote: string | null;
}

/// One page of key names read with plain SCAN, without an index.
export interface KeysPage {
  keys: KeyRef[];
  /// Every node was walked to the end.
  done: boolean;
  calls: number;
  /// Keys handed out since the walk began.
  returned: number;
  /// About how many keys the server has walked since the walk began.
  examined: number;
  node: string | null;
  nodeAt: number;
  nodes: number;
  mode: "glob" | "substring" | null;
  tookMs: number;
}

export interface SearchRow {
  name: string;
  raw: string | null;
  ttl: number | null;
  mem: number | null;
}

export interface SearchPage {
  mode: SearchMode;
  rows: SearchRow[];
  done: boolean;
  cancelled: boolean;
  viaFts: boolean;
  fts: boolean;
  ftsApplicable: boolean;
  examined: number;
  tookMs: number;
  verifiable: boolean;
}

export interface VerifyReport {
  calls: number;
  examined: number;
  matched: number;
  added: number;
  removed: number;
  complete: boolean;
  truncated: boolean;
  tookMs: number;
}

export interface FtsProgress {
  session: number;
  phase: "building" | "ready" | "dropping" | "dropped" | "cancelled" | "failed";
  done: number;
  total: number;
  error: string | null;
}

export const PREVIEW_BYTES = 65536;
export const CHUNK_BYTES = 1 << 20;
/// Largest string the viewer will hold in full (CONCEPT: the webview never holds more than it shows).
export const MAX_FULL_BYTES = 32 << 20;

export type MutationOp =
  | { op: "delete" }
  | { op: "rename"; to: string }
  | { op: "expire"; seconds: number }
  | { op: "persist" }
  | { op: "setString"; value: string; keepTtl: boolean }
  | { op: "hashSet"; field: string; fieldRaw?: string | null; value: string }
  | { op: "hashDel"; field: string; fieldRaw?: string | null }
  | { op: "setAdd"; member: string }
  | { op: "setRem"; member: string; memberRaw?: string | null }
  | { op: "zAdd"; member: string; memberRaw?: string | null; score: number }
  | { op: "zRem"; member: string; memberRaw?: string | null }
  | { op: "listPush"; value: string; head: boolean }
  | { op: "listSet"; index: number; value: string }
  | { op: "listRem"; index: number }
  | { op: "streamAdd"; fields: [string, string][] }
  | { op: "streamDel"; id: string };

export interface MutateResult {
  command: string;
  exists: boolean;
  ttl: number | null;
  reply: string;
  indexAdded: number;
  indexRemoved: number;
  queued: boolean;
  tookMs: number;
}

/// What a pattern delete removes: the matches of a typed `query`, or, when
/// it was started on a tree node, every key under exactly that `prefix`
/// (`query` is then the text shown).
export interface DeleteTarget {
  query: string;
  prefix: KeyRef | null;
}

export interface DeletePreview {
  mode: SearchMode;
  count: number;
  sample: string[];
  tookMs: number;
}

export interface DeleteProgress {
  session: number;
  query: string;
  phase: "counting" | "deleting" | "paused" | "complete" | "cancelled" | "failed";
  total: number | null;
  matched: number;
  deleted: number;
  removed: number;
  calls: number;
  elapsedMs: number;
  error: string | null;
}

export interface IndexChanged {
  session: number;
  reason: "mutate" | "delete" | "flush" | "live";
  added: number;
  removed: number;
}

export interface MemoryStats {
  backendRss: number;
  webviewRss: number;
  processes: string[];
}

export interface StartupOptions {
  autoconnect: string | null;
  tag: string;
  autoexpand: string | null;
  autoselect: KeyRef[];
  autosearch: string[];
  autofts: boolean;
  automutate: string | null;
  autodelete: string | null;
  autolive: boolean;
  autoscanopts: string | null;
  automode: string | null;
  autoconsole: string[];
  autoswitchdb: number | null;
  autoprofile: string | null;
  autoprofileSsh: string | null;
}

export const api = {
  log: (level: "info" | "warn" | "error", message: string) => invoke<void>("app_log", { level, message }).catch(() => {}),
  startupOptions: () => invoke<StartupOptions>("startup_options"),
  connect: (url: string, tag: string, profile: ScanProfile | null = null, target: ScanTarget | null = null, indexMode: string | null = null) =>
    invoke<Connection>("connect", { url, tag, profile, target, indexMode }),
  profilesList: () => invoke<StoredProfile[]>("profiles_list"),
  /// `password`: null keeps the stored one, "" removes it.
  /// `sshSecret` follows the same three answers as `password`.
  profileSave: (profile: ProfileInput, password: string | null, sshSecret: string | null = null) =>
    invoke<Omit<StoredProfile, "indexBytes">>("profile_save", { profile, password, sshSecret }),
  /// Record an SSH host key the user read and accepted.
  sshTrust: (key: HostKey) => invoke<void>("ssh_trust", { key }),
  profileDelete: (id: string) => invoke<void>("profile_delete", { id }),
  profileClearIndex: (id: string) => invoke<void>("profile_clear_index", { id }),
  profileFromUrl: (url: string) => invoke<UrlParts>("profile_from_url", { url }),
  connectProfile: (profileId: string) => invoke<Connection>("connect_profile", { profileId }),
  disconnect: (id: number) => invoke<void>("disconnect", { id }),
  databases: (id: number) => invoke<Databases>("databases", { id }),
  /// Replaces the session: the returned connection has a new id.
  switchDb: (id: number, db: number) => invoke<Connection>("switch_db", { id, db }),
  /// Scan everything. On an on-demand index this needs `confirmed`.
  scanStart: (id: number, confirmed = false) => invoke<void>("scan_start", { id, confirmed }),
  /// Scan the subtree under a prefix, now or after the scans in line.
  scanSubtree: (id: number, prefix: KeyRef, confirmed: boolean) =>
    invoke<"started" | "queued" | "covered">("scan_subtree", { id, prefix, confirmed }),
  /// Stop the running scan and drop the waiting subtree scans.
  scanCancel: (id: number) => invoke<void>("scan_cancel", { id }),
  /// Replaces the session: the returned connection has a new id.
  setIndexMode: (id: number, mode: IndexMode | null) => invoke<Connection>("set_index_mode", { id, mode }),
  /// One typed line through the console. `confirmed` answers a confirmation
  /// an earlier call with the same line asked for.
  consoleRun: (id: number, line: string, node: string | null, confirmed: string | null) =>
    invoke<ConsoleReply>("console_run", { id, line, node, confirmed }),
  /// The next page of a walk without an index; `restart` begins a new one.
  keysPage: (id: number, pattern: string | null, restart: boolean) => invoke<KeysPage>("keys_page", { id, pattern, restart }),
  scanPause: (id: number) => invoke<void>("scan_pause", { id }),
  scanResume: (id: number) => invoke<void>("scan_resume", { id }),
  listRows: (id: number, offset: number, limit = WINDOW) => invoke<ListWindow>("list_rows", { id, offset, limit }),
  keyMeta: (id: number, names: KeyRef[]) => invoke<KeyMeta[]>("key_meta", { id, names }),
  memoryStats: () => invoke<MemoryStats>("memory_stats"),
  treeRoot: (id: number) => invoke<TreeRoot>("tree_root", { id }),
  treeChildren: (
    id: number,
    prefix: string,
    prefixRaw: string | null,
    after: TreeAfter | null,
    sort: TreeSort,
    filter: ChildFilter | null = null,
    limit = WINDOW,
  ) => invoke<TreeWindow>("tree_children", { id, prefix, prefixRaw, after, limit, sort, filter }),
  treeCount: (id: number, prefix: string, prefixRaw: string | null, filter: ChildFilter) =>
    invoke<number>("tree_count", { id, prefix, prefixRaw, filter }),
  treeFold: (id: number, prefix: string, prefixRaw: string | null) => invoke<FoldInfo>("tree_fold", { id, prefix, prefixRaw }),
  treeFoldRule: (id: number, prefix: string, prefixRaw: string | null, rule: FoldRule) =>
    invoke<void>("tree_fold_rule", { id, prefix, prefixRaw, rule }),
  valueInspect: (id: number, key: KeyRef) => invoke<KeyInfo>("value_inspect", { id, key }),
  valuePage: (id: number, key: KeyRef, keyType: string, cursor: ValueCursor | null, filter: string | null, limit = WINDOW) =>
    invoke<ValuePage>("value_page", { id, key, keyType, cursor, limit, filter }),
  valueChunk: (id: number, key: KeyRef, offset: number, len: number) =>
    invoke<ArrayBuffer>("value_chunk", { id, key, offset, len }).then((b) => new Uint8Array(b)),
  valueDump: (id: number, key: KeyRef) => invoke<DumpInfo>("value_dump", { id, key }),
  search: (id: number, query: string, after: string | null, afterRaw: string | null, fresh: boolean, limit = WINDOW) =>
    invoke<SearchPage>("search", { id, query, after, afterRaw, limit, fresh }),
  searchCount: (id: number, query: string) => invoke<number | null>("search_count", { id, query }),
  searchCancel: (id: number) => invoke<void>("search_cancel", { id }).catch(() => {}),
  searchFts: (id: number, enable: boolean) => invoke<void>("search_fts", { id, enable }),
  searchVerify: (id: number, query: string) => invoke<VerifyReport>("search_verify", { id, query }),
  onFtsProgress: (handler: (p: FtsProgress) => void): Promise<UnlistenFn> =>
    listen<FtsProgress>("search:fts", (e) => handler(e.payload)),
  mutate: (id: number, key: KeyRef, op: MutationOp, confirmed: boolean) =>
    invoke<MutateResult>("mutate", { id, key, op, confirmed }),
  deletePreview: (id: number, target: DeleteTarget) => invoke<DeletePreview>("delete_preview", { id, query: target.query, prefix: target.prefix }),
  deleteStart: (id: number, target: DeleteTarget, confirmed: boolean) =>
    invoke<void>("delete_start", { id, query: target.query, prefix: target.prefix, confirmed }),
  deletePause: (id: number) => invoke<void>("delete_pause", { id }),
  deleteResume: (id: number) => invoke<void>("delete_resume", { id }),
  deleteCancel: (id: number) => invoke<void>("delete_cancel", { id }),
  flush: (id: number, all: boolean, confirm: string) => invoke<void>("flush", { id, all, confirm }),
  connectionInfo: (id: number) => invoke<Connection>("connection_info", { id }),
  scanOptions: (id: number, profile: ScanProfile, target: ScanTarget) => invoke<void>("scan_options", { id, profile, target }),
  notifySetting: (id: number) => invoke<NotifySetting>("notify_setting", { id }),
  notifyEnable: (id: number, confirmed: boolean) => invoke<string>("notify_enable", { id, confirmed }),
  liveSet: (id: number, on: boolean) => invoke<LiveStatus>("live_set", { id, on }),
  liveStatus: (id: number) => invoke<LiveStatus>("live_status", { id }),
  onLiveStatus: (handler: (p: LiveStatus) => void): Promise<UnlistenFn> =>
    listen<LiveStatus>("live:status", (e) => handler(e.payload)),
  onDeleteProgress: (handler: (p: DeleteProgress) => void): Promise<UnlistenFn> =>
    listen<DeleteProgress>("delete:progress", (e) => handler(e.payload)),
  onIndexChanged: (handler: (p: IndexChanged) => void): Promise<UnlistenFn> =>
    listen<IndexChanged>("index:changed", (e) => handler(e.payload)),
  onScanProgress: (handler: (p: ScanProgress) => void): Promise<UnlistenFn> =>
    listen<ScanProgress>("scan:progress", (e) => handler(e.payload)),
};

export function fmtCount(n: number): string {
  return n.toLocaleString("en-US");
}

export function fmtBytes(b: number | null): string {
  if (b === null) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = b;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return u === 0 ? `${b} B` : `${v.toFixed(1)} ${units[u]}`;
}

export function fmtTtl(ttl: number | null): string {
  if (ttl === null) return "";
  if (ttl < 60) return `${ttl}s`;
  if (ttl < 3600) return `${Math.round(ttl / 60)}m`;
  if (ttl < 86400) return `${Math.round(ttl / 3600)}h`;
  return `${Math.round(ttl / 86400)}d`;
}
