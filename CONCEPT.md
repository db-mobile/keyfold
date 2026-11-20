# Concept: Browsing a Redis database with millions of keys

Status: draft, 2026-10-25
Target: a Tauri 2 desktop client (Rust backend, SolidJS frontend)
Servers: Redis and Valkey, supported equally

## 1. The problem

Redis gives you exactly one way to enumerate keys without blocking the server: `SCAN`.
`SCAN` is a cursor over the hash table. It is unordered, has no random access,
cannot count, cannot sort, cannot tell you which namespaces exist, and may return
duplicates. Every feature users expect from a key browser (a namespace tree with
counts, sorted lists, substring search, "jump to page 300") has no server-side
support at all.

Existing clients resolve this tension in one of two bad ways:

| Strategy | Examples | What breaks |
|---|---|---|
| Scan everything into the UI process | RDM/RESP.app, Tiny RDM (tree mode) | RAM explodes (JS strings, DOM, tree objects per key), UI freezes, 10M keys is unusable |
| Scan one page at a time | RedisInsight, Medis | Tree only shows what happened to be scanned so far, counts are meaningless, no sort, no search beyond glob, "load more" forever |

Neither is wrong about Redis. They are wrong about *where* the complete picture
should live. The complete picture must exist somewhere for the tree to be honest,
but it must not live in the webview, and it must not block anything.

## 2. Design principles

1. **Usable in 100 ms, complete in the background.** Connect, see the first keys
   immediately, watch the tree fill in. Never a spinner over a blank screen.
2. **The webview never holds the keyspace.** It holds a window of a few hundred
   rows. Everything else lives in the Rust backend, bounded by a disk-backed index.
3. **Redis is the truth, the index is a cache.** Every number is labelled with how
   fresh it is. Opening a key always talks to Redis.
4. **Be gentle to the server.** Never `KEYS`. Adaptive `SCAN COUNT`. Prefer
   replicas for scanning. Pausable. Visible cost.
5. **High cardinality is the normal case.** A namespace with 2 million children
   must be as pleasant as one with 12.
6. **Big values are paged like big keyspaces.** A 5 million member set never
   arrives in one piece.
7. **Redis and Valkey are equals.** No feature is gated on a Redis version
   string, no type viewer assumes one module ecosystem, and both servers are in
   every CI run.

## 3. The core idea: a local keyspace index

The backend builds a **local index of key names** (not values) by running a
background `SCAN`. The index is stored in an embedded SQLite database on disk, one
file per connection + logical db (storage layout in section 3.4). The UI never
queries Redis to render the tree or a list. It queries the index through narrow,
windowed commands.

Why this resolves the tension:

- The tree is complete and the counts are exact once the scan finishes. While it
  runs, the UI shows progress and marks counts as lower bounds.
- RAM is bounded by SQLite's page cache, not by key count. 10M keys of 40 bytes is
  roughly 600 MB on disk and a configurable 64 to 256 MB in RAM.
- The index persists. Reopening the app shows last session's tree instantly, with
  a "last scanned 2 days ago" badge and a refresh running in the background.
- Search, sort, and counts run locally against a B-tree. No server load.

Why SQLite and not an in-memory trie:

- A radix tree over 10M keys is fast but lives entirely in RAM and is gone at exit.
- SQLite gives persistence, bounded memory, range scans (prefix enumeration),
  optional trigram full-text search, and is a single bundled dependency (`rusqlite`).
- Tree expansion becomes an indexed lookup, so a tree node with 2M children opens
  in milliseconds (only a window is read).

### 3.1 Schema

```sql
-- every key seen, keyed by name
CREATE TABLE keys (
  name      BLOB PRIMARY KEY,    -- raw bytes; Redis keys are binary-safe
  type      INTEGER,             -- from SCAN TYPE or lazy TYPE, null = unknown
  seen_at   INTEGER NOT NULL,    -- scan generation that last saw it
  ttl       INTEGER,             -- cached, lazily refreshed, null = unknown
  mem       INTEGER              -- cached MEMORY USAGE, null = unknown
) WITHOUT ROWID;

-- materialised namespace tree so tree expansion is O(window), not O(subtree)
CREATE TABLE nodes (
  parent    BLOB NOT NULL,       -- full path of parent, '' for root
  segment   BLOB NOT NULL,       -- this level's segment
  leaf      INTEGER NOT NULL,    -- 1 if an actual key ends here
  count     INTEGER NOT NULL,    -- keys under this node (exact after scan completes)
  seg_num   INTEGER,             -- numeric value when segment is all digits (numeric sort, range buckets)
  children  INTEGER NOT NULL DEFAULT 0,  -- distinct direct children
  -- per-class counters of distinct direct children, maintained at ingestion (section 5.6)
  c_uuid INTEGER DEFAULT 0, c_ulid INTEGER DEFAULT 0, c_hex INTEGER DEFAULT 0,
  c_num INTEGER DEFAULT 0,  c_date INTEGER DEFAULT 0, c_opaque INTEGER DEFAULT 0,
  c_email INTEGER DEFAULT 0, c_literal INTEGER DEFAULT 0,
  len_min INTEGER, len_max INTEGER,
  PRIMARY KEY (parent, segment)
) WITHOUT ROWID;
CREATE INDEX nodes_num ON nodes(parent, seg_num) WHERE seg_num IS NOT NULL;

-- lazily computed, cached fold details (aggregated sub-structure, similarity)
CREATE TABLE fold_stats (
  path        BLOB PRIMARY KEY,  -- the folded level, e.g. 'user:{n}'
  similarity  REAL,
  computed_at INTEGER,
  base_count  INTEGER,           -- subtree count at computation, for invalidation
  summary     BLOB               -- aggregated children with counts and coverage
);

CREATE TABLE meta (k TEXT PRIMARY KEY, v);  -- schema_version, delimiter, generation, dbsize, scanned_at ...
```

`nodes` is maintained during ingestion: each SCAN batch is split by delimiter in
memory, aggregated into a `HashMap<(parent, segment), delta>`, then flushed in one
transaction. 10M keys at depth 3 is 30M upserts, batched into roughly 5000
transactions. SQLite handles that in well under a minute on an SSD, which is
faster than the network will deliver the keys anyway.

The `nodes` table is what makes the "scan a page at a time" clients' problem go
away: the tree is a real data structure, not a byproduct of whatever happened to
be loaded.

### 3.2 Memory and disk budget

| Keys | Index on disk | Backend RAM (cache) | Webview RAM |
|---|---|---|---|
| 100k | ~8 MB | < 50 MB | < 100 MB |
| 1M | ~70 MB | ~100 MB | < 100 MB |
| 10M | ~600 MB | ~200 MB (cap) | < 100 MB |
| 100M | ~6 GB | ~200 MB (cap) | < 100 MB |

Above a user-set threshold (default 20M keys, read from `DBSIZE` before scanning)
the app asks before indexing and offers "index only what I expand" instead
(see section 7, modes).

### 3.3 Trigram full-text search (opt-in per connection)

The B-tree on `keys.name` makes prefix queries a range scan, but a substring
query such as `session` has no entry point and must test every row: about one
second at 10M keys, growing linearly.

A trigram index splits every key into its overlapping three-character windows
(`user:42:session` yields `use`, `ser`, `er:`, `r:4`, ... `ion`) and records
which keys contain each. A substring search becomes an intersection of a few
posting lists, independent of key count, followed by verification of the
candidates. SQLite's FTS5 ships this as the `trigram` tokenizer:

```sql
CREATE VIRTUAL TABLE keys_fts USING fts5(
  name, tokenize = 'trigram', content = 'keys', content_rowid = 'rowid'
);
```

`LIKE '%session%'` and `GLOB` queries against the FTS table use the index
automatically and are case-insensitive. Rows are inserted in the same
transactions as `keys` during ingestion.

Costs:

| Aspect | Effect |
|---|---|
| Disk | 2 to 3 times the plain key index; 10M keys grow from ~600 MB to 1.5 to 2 GB |
| Ingestion | Full scans take 30 to 50 percent longer on disk-bound machines |
| Query length | Queries under three characters cannot use it and fall back to the row scan |
| Common trigrams | A query that matches most keys (`user` where every key starts with `user:`) yields huge posting lists and is no faster than the scan; prefix-shaped queries keep going to the B-tree |

Because the whole index is a rebuildable cache, the FTS table can be created or
dropped at any time from the existing `keys` table with no server traffic.

### 3.4 Storage: where profiles, indexes and secrets live

Three kinds of data, three homes, chosen by what each one is:

| Data | Location | Why |
|---|---|---|
| Connection profiles | Tauri app data dir (`~/.local/share/<bundle-id>` on Linux, equivalents on macOS and Windows) | Small, user-authored, must survive |
| Key indexes | Tauri app cache dir (`~/.cache/<bundle-id>` on Linux), folder per connection id, one SQLite file per logical db | Large and rebuildable. Cache dirs are excluded from backups on macOS and treated as deletable by cleanup tools. OS purging under storage pressure costs a rescan, which is the right trade for gigabyte files |
| Credentials | OS keychain via a Tauri plugin | Never on disk in either directory. A connection reached through an SSH bastion has a second entry for the SSH password or key passphrase |
| SSH host keys the user accepted | `known_hosts` in the app data dir | The user's own `~/.ssh/known_hosts` is read, never written |

Rules:

- The folder name is the connection's stable UUID from the profile, never its
  display name or host, so renaming a connection keeps its index.
- Deleting a profile deletes its index folder. "Clear index" per connection
  drops the folder; the next connect rescans.
- The profile list shows each connection's index size on disk.
- One global setting moves the cache root to another disk. Indexes for several
  10M-key databases reach tens of gigabytes.
- Index files are created with user-only permissions. Key names often carry
  emails, customer ids and tenant names.
- Each profile has an **ephemeral index** option: SQLite in memory or in a temp
  file, deleted on disconnect. Default on for connections tagged prod, off
  otherwise.
- `meta.schema_version` is checked on open. An outdated index is dropped and
  rebuilt rather than migrated; it is a cache.

## 4. The scan engine

Runs as a Tokio task per connection, in the Rust backend. Emits progress events to
the UI.

- **Progress is exact.** `DBSIZE` is O(1) and gives the total. Progress bar reads
  "1,204,118 of 5,391,002 keys". A cursor-based estimate is the fallback when
  `DBSIZE` is unavailable (ACL restricted).
- **Adaptive COUNT.** Start at 1000. Measure round trip. Grow towards 10000 while
  server-side time per call stays under a few milliseconds, shrink when latency
  rises. Over a 30 ms WAN link, 5M keys at COUNT 5000 is 1000 round trips, about
  30 seconds. Locally it is a few seconds.
- **Gentleness slider.** Three positions: fast (continuous), normal (small sleep
  between calls, back off if `INFO` shows rising `instantaneous_ops_per_sec` or
  slow log entries), polite (replica only, 50 ms gaps). Production connections
  default to normal.
- **Prefer replicas.** If the connection resolves to a replica or the cluster has
  replicas, scan them with `READONLY`. The user can see which node is being
  scanned and switch.
- **Cluster.** One cursor per master (or replica), scanned concurrently, written
  to the same index with a node tag. Progress is the sum.
- **Type filter.** Use `SCAN ... TYPE` (Redis 6+) only when the user asks for a
  typed view. The default scan fetches names only and leaves `type` null. Types
  are fetched lazily for visible rows with pipelined `TYPE` calls.
- **Generations.** Each full scan has a generation number. Keys not seen in the
  completed generation are deleted from the index. A partial scan never deletes.
- **Duplicates are normal.** Ingestion is upsert. Counts in `nodes` are only
  incremented when a key is actually new to the index.
- **Pausable and resumable.** The cursor is persisted every batch. Closing the app
  mid-scan and reopening resumes from the cursor, with a staleness warning
  (keys created during the pause may be missed; that is a SCAN property anyway).
- **Subtree rescan.** Right-click any node: "Rescan `user:*`". Runs
  `SCAN MATCH user:*`. The server still walks the full hash table, but nothing
  non-matching crosses the network, so it is cheap on bandwidth and the index for
  that subtree becomes exact again. Shown honestly in the UI as "server-side
  cost: full walk".

Never used by the browser: `KEYS`, `DEBUG`, `MONITOR`. The command console
(section 8) is the separate tool for what the user asks for explicitly: it
sends what the user types, under the user's own ACL, and asks before `KEYS`
or `DEBUG`. `MONITOR` and pub/sub streams are a later tool; the console
refuses them.

## 5. Browsing UX

Three navigation surfaces share one index and one selection.

### 5.1 Tree

- Delimiter defaults to `:`, auto-detected on first scan by sampling 10k keys for
  the most frequent separator among `: / . _ |`. Changing the delimiter rebuilds
  `nodes` from `keys` locally without rescanning Redis.
- Expanding a node reads one window (first 200 children, sorted) from `nodes`.
  Scrolling reads further windows. Rendering is virtualised; the DOM holds about
  60 rows regardless of node size.
- Every node shows its count. While scanning: "≥ 14,203". After: "14,203".
  Hovering shows "counted 3 min ago".
- **Folding of high-cardinality levels.** This is the feature that turns
  "millions of keys" from a burden into a non-event. When a level's children
  are generated identifiers, they collapse into one virtual node that shows the
  structure beneath them, aggregated across all ids:

  ```
  ▸ user              2,104,331 keys
      count                   1
      index                   1
      {n}  1 … 2,104,331  e.g. 4821, 1093773, 55        [filter…]
        ▸ profile          2,104,331   100 %
        ▸ sessions         1,872,004    88.9 %
        ▸ cart               310,920    14.8 %
  ```

  The full design is in section 5.6.
- Sorting of children: by name, by count, or by cached memory (when sampled).
  Numeric segments sort numerically.

### 5.2 Flat list

The same index as a sorted, virtualised, infinitely scrollable list. Column
sort by name, type, TTL, memory. Typed columns fill in lazily for visible rows
(pipelined `TYPE`, `TTL`, `MEMORY USAGE` with a 150 ms debounce, batched per
scroll stop, cached in the index). Scrolling fast never issues requests.

### 5.3 Search

One search box, three interpretations, chosen by syntax and shown as a chip:

| Input | Mode | Executes against |
|---|---|---|
| `user:42:*` | glob | index (prefix range + glob filter), instant |
| `session` | substring | index, trigram FTS if enabled for the connection (section 3.3), else `LIKE` scan (~1 s at 10M) |
| `/user:\d+:cart/` | regex | index, streamed, cancellable |
| `user:42:profile` (exact, Enter) | jump | Redis directly, no index needed |

If the index is stale or partial, the result header offers "verify against
server" which runs a bounded `SCAN MATCH` and reconciles.

When a substring search without trigram FTS takes longer than 500 ms, the result
header offers "enable instant substring search for this connection", which
builds the FTS index from the existing `keys` table in the background without
touching the server.

### 5.4 Jump to key (Ctrl+K)

Opens any exact key name directly from Redis. Works with no index at all, works
on a 100M-key database, and is the fastest path for people who know what they
are looking for. Recently opened keys are listed first.

### 5.5 Cold start timeline

| Time | What the user sees |
|---|---|
| 0 ms | Connection opens, `INFO` + `DBSIZE` + first `SCAN` page pipelined |
| < 100 ms | Header: version, role, 5.39M keys, 2.1 GB used. First 200 keys visible. Tree shows top-level namespaces seen so far. |
| 100 ms+ | Progress bar in the sidebar. Tree counts grow live (throttled to 4 updates per second). |
| if index exists | Full tree from last session shown at 0 ms with a "stale, refreshing" badge. |
| scan done | Counts lose the "≥", badge reads "scanned just now". |

### 5.6 Folding: detailed design

Every fold decision is made on exact counts from the index, never on a loaded
page.

**Signals.** Three, from cheapest to most expensive:

1. *Cardinality.* Never considered under 50 distinct children, always
   considered above 2,000.
2. *Segment shape.* Each distinct child segment is classified once, at
   ingestion, into the first matching class:

   | Class | Test | Fold label |
   |---|---|---|
   | uuid | 36 chars, 8-4-4-4-12 hex | `{uuid}` |
   | ulid | 26 chars, Crockford base32 | `{ulid}` |
   | hex | all hex, uniform length in 16, 24, 32, 40, 64 | `{hex32}` etc. |
   | numeric | all digits | `{n}`; `{unix-ts}` when 10 or 13 digits in a plausible date range |
   | date | starts with `YYYY-MM-DD` | `{date}` |
   | opaque | alphanumerics plus `-` `_`, uniform length ≥ 8, mixed case or digits | `{id}` |
   | email | contains `@` and a dot | `{email}` |
   | literal | everything else | never folded by shape |

   Counters per class live on the parent row in `nodes` and are incremented
   when a new distinct child is first inserted, so classification costs
   nothing at expansion time.
3. *Structural similarity.* Average Jaccard overlap of the child-segment sets
   beneath a sample of 200 children. Catches entity levels whose ids have no
   recognisable shape (usernames, slugs). Computed lazily, cached in
   `fold_stats`.

**Decision rule.** For a parent with C distinct children:

- A class with ≥ 100 members covering ≥ 90 % of C is folded. Remaining
  literals (`user:count`, `user:index`) stay as ordinary siblings next to the
  fold. Mixed levels are the common case and must never degrade to
  all-or-nothing.
- Several generated classes that together cover ≥ 90 % produce one fold per
  class with ≥ 100 members (`order:{n}` and `order:{hex32}` side by side).
- Otherwise, if C ≥ 2,000 and similarity ≥ 0.6, the level folds as `{*}`.
- Otherwise no fold; the level is a virtualised sorted list.

Refinements:

- *Affix stripping.* When ≥ 90 % of children share a common prefix or suffix
  (`sess_a1b2…`, `ORD-2024-00001`), strip it, classify the remainder, label
  the fold `sess_{hex32}`. Computed lazily on a sample, only for levels where
  literal dominates and C ≥ 100.
- *Hysteresis.* A level folds at 100 and unfolds only below 50. The decision
  is taken at expansion and held until collapse, so a tree never flickers while
  a scan is running.
- *Invalidation.* `fold_stats` is recomputed when the subtree count drifts more
  than 5 % from `base_count` or after a subtree rescan.

**Inside a folded node.**

- *Aggregated structure beneath*, with coverage: `profile` 100 %, `cart`
  14.8 %. Computed from `nodes` rows one level deeper whose parent falls in
  the fold's range, aggregated by segment in Rust, cached in `fold_stats`.
  Folds nest recursively: `tenant:{n}:user:{n}:session:{uuid}`.
- *Filter box.* A complete id descends into that concrete node; a prefix
  narrows the fold with a live count; a regex is accepted.
- *Buckets* for browsing by clicking. Numeric ids bucket by range (1 to 999,
  1,000 to 9,999, ...) via the `seg_num` index. Hex, uuid and ulid ids bucket
  by leading character, 16 or 32 per level, each bucket folding further.
- *Sample and range in the row itself*: `1 … 2,104,331`, `e.g. 4821, 1093773,
  55`.
- *"Show as list"* expands the ids as a plain virtualised list, sorted
  numerically where applicable.
- *"Why folded?"* tooltip with the evidence: 98.7 % numeric, 2.1M distinct.

**Overrides.** Rules on the connection profile keyed by path pattern: rename a
level (`{userId}`), force a fold, forbid a fold, declare a custom segment regex
as an id class. Thresholds (50 / 100 / 2,000, 90 %, 0.6) are global settings.
Heuristics propose, the user corrects, the correction is remembered.

**Later.** Folding search results the same way, so `user:*:cart` returns one
row `user:{n}:cart` with a count instead of two million hits.

## 6. Values

- Nothing is fetched on hover. Selection fetches `TYPE`, `TTL`, `MEMORY USAGE`,
  `OBJECT ENCODING` in one pipeline.
- Collections are paged with their cursor commands: `HSCAN`, `SSCAN`, `ZSCAN`
  (plus `ZRANGE` windows for sorted views), `LRANGE` windows, `XRANGE` with
  bounded count. The value panel shows "showing 200 of 4,113,920 fields" and
  scrolls into further pages. Field-level search uses `HSCAN MATCH`.
- Strings over 1 MB load a preview (`GETRANGE 0 65535`), then "load full" streams
  the rest in chunks. Detection of JSON, msgpack, gzip, protobuf and binary runs
  on the preview and picks the viewer.
- Large values cross the Tauri IPC as raw bytes (`tauri::ipc::Response`), not as
  JSON-encoded strings, and are rendered by a virtualised viewer.

## 7. Truth, staleness, and modes

**Write-through.** Every mutation the app performs (set, delete, rename, expire,
type conversion) updates the index in the same action, so the tree never shows
a key the user just deleted.

**Verify on open.** Opening a key runs `EXISTS` implicitly through the pipeline
above. A missing key is removed from the index and the row shows "gone".

**Keyspace notifications (opt-in).** If `notify-keyspace-events` is enabled on
the server, the app can subscribe to `__keyevent@N__:*` and keep the index live
for new, deleted, renamed and expired keys. Off by default because it costs
server CPU and requires configuration. When on, the staleness badge reads
"live".

**Three modes per connection:**

| Mode | When | Behaviour |
|---|---|---|
| Full index (default) | `DBSIZE` under threshold | Background scan to completion, persisted |
| On-demand index | Huge db, or user choice | Only subtrees the user expands are scanned (`SCAN MATCH prefix*`), each with its own freshness. Root shows namespaces seen so far plus a "scan root" action. |
| No index | Locked-down prod, read-only review | Plain `SCAN` pages with "load more", jump-to-key, no tree counts. This is the RedisInsight experience, as an explicit floor rather than the ceiling. |

## 8. Safety

- Deleting a pattern goes through the index: exact count and a 20-key sample are
  shown before confirming, deletion runs as batched `UNLINK` in the background
  with pause and cancel, and the index is updated as it goes.
- `FLUSHDB` / `FLUSHALL` require typing the database name.
- Connections are tagged (dev / staging / prod, colour coded). Prod defaults to
  the polite scan profile, an ephemeral index (section 3.4), and confirms every
  write.
- Credentials are stored in the OS keychain, never in profile or index files.
- All read commands used for browsing are listed in the docs so operators can
  write a matching ACL (`SCAN`, `TYPE`, `TTL`, `MEMORY`, `OBJECT`, `DBSIZE`,
  `INFO`, `CLUSTER`, the per-type read commands). The app degrades cleanly when
  a command is denied.
- **Command console** (amended 2026-11-22). A panel where the user types a
  command and reads the reply. Its rules:
  - It has its own connection. Nothing typed there can change the state of
    the connections the browser uses.
  - It runs commands that answer once. Commands that change connection
    state (`SELECT`, `MULTI`, `AUTH`, `HELLO`, `RESET`, `CLIENT REPLY`, ...),
    that block, or that stream (`SUBSCRIBE`, `MONITOR`) are refused with the
    reason.
  - What a command is comes from the server (`COMMAND INFO`: flags and ACL
    categories), never from a version string. Without that answer every
    command counts as a write.
  - Prod confirms every command that is not known to be read-only. `KEYS`
    and everything the server calls dangerous ask on every tag and say what
    they cost. `FLUSHALL`, `FLUSHDB`, `SHUTDOWN`, `SWAPDB` and `DEBUG`
    require typing the connection's name. The backend enforces all of it.
  - The index follows: the keys a command touched (`COMMAND GETKEYS`) are
    read again and written through. A write that names no keys marks the
    index as possibly stale and offers a rescan.
  - Replies are cut to a bounded number of lines before they reach the
    webview, with every cut marked.
- **SSH tunnels** (amended 2026-11-22). A stored connection can name an SSH
  bastion. The host key is verified against the user's `known_hosts` and the
  app's own; an unknown key stops the connect and shows its fingerprint, a
  changed key is refused. The SSH password or key passphrase lives in the OS
  keychain like the server password.

## 9. Performance targets (acceptance criteria)

Measured on a 10M-key database, 40-byte average key, local network:

| Operation | Target |
|---|---|
| Connect to first keys visible | < 150 ms |
| Expand any tree node (including 2M children) | < 50 ms |
| Scroll a flat list at any position | 60 fps, no request storms |
| Glob / prefix search | < 50 ms |
| Substring search, no FTS | < 1.5 s, cancellable |
| Substring search, trigram FTS | < 100 ms |
| Full scan, local | < 60 s |
| Backend RSS during and after scan | < 300 MB |
| Webview RSS | < 150 MB |
| Open a 1M-field hash | < 100 ms to first page |

## 10. Architecture sketch

```
┌──────── webview (SolidJS + TS) ────────┐
│ tree / list / search / value viewers   │   windows of ≤ 200 rows
│ virtualised; holds no keyspace         │   events: scan progress, index changed
└──────────────┬─────────────────────────┘
               │ Tauri invoke / events / raw binary responses
┌──────────────┴─────────────────────────┐
│ Rust backend                           │
│  connection manager  (redis crate,     │
│    one conn per node, sentinel, TLS,   │
│    optional SSH tunnel)                │
│  capability layer    (HELLO/INFO +     │
│    COMMAND probing, type registry)     │
│  scan engine         (tokio task, one  │
│    per connection, adaptive, pausable) │
│  indexer             (SQLite via       │
│    rusqlite; keys + nodes + fts)       │
│  query API           (children(), rows │
│    (), search(), fold_stats())         │
│  value service       (paged reads,     │
│    pipelined metadata, write-through)  │
└──────────────┬─────────────────────────┘
               │ RESP3
     Redis / Valkey, standalone, cluster, sentinel
```

### 10.1 Server parity: Redis and Valkey

Valkey forked from Redis 7.2.4 and shares the wire protocol and every command
the browser depends on. The two diverge in places that a client must handle
deliberately:

| Divergence | Handling |
|---|---|
| Valkey reports `redis_version: 7.2.4` forever, plus `server_name: valkey` and `valkey_version` | Never gate features on `redis_version`. The capability layer reads `HELLO` and `INFO server` for the server name and version, then probes `COMMAND INFO` for each optional command (`SCAN TYPE`, `MEMORY USAGE`, `OBJECT ENCODING`, `UNLINK`, `CLUSTER SHARDS`, ...). Results are cached per connection. |
| Redis 8 folded JSON, search, time series, probabilistic types and vector sets into core; Valkey ships its own JSON, Bloom and search modules with similar but not identical commands and `TYPE` names | The value viewer dispatches on the exact `TYPE` string through a type registry that maps both ecosystems to viewers. Unknown types fall back to a raw view with `DUMP` size and encoding, never an error. |
| Valkey 9 supports multiple databases in cluster mode; Redis cluster is db 0 only | The backend owns cluster topology: it reads `CLUSTER SHARDS` (or `CLUSTER SLOTS`), opens one connection per node, and routes by slot itself. The crate's cluster abstraction is not used. Database selection is a per-node `SELECT` where the capability layer says it is allowed. |
| Module command surfaces evolve independently | Module commands are isolated behind the type registry, so adding a type is a table entry plus a viewer, not a change to the core. |

CI runs the full backend test suite against containers for **Redis 7.2, Redis
8.x, Valkey 8.x and Valkey 9.x**. A feature is not done until it passes on all
four. The matrix is extended when either project ships a new major.

Product naming must not include "Redis". The trademark is actively enforced and
the app serves both servers equally.

Backend command surface, deliberately small and always windowed:

```
connect(profile) -> ConnectionId
scan_start / scan_pause / scan_resume / scan_subtree(prefix)
tree_children(conn, db, path, offset, limit, sort)   -> [Node]
list_rows(conn, db, offset, limit, sort, filter)     -> [Row]
search(conn, db, query, mode, limit, cursor)         -> SearchPage
fold_stats(conn, db, path)                           -> FoldSummary
key_meta(conn, db, names[])                          -> [Meta]    (pipelined)
value_page(conn, db, name, cursor, limit, filter)    -> ValuePage
mutate(conn, db, op)                                 -> ()        (write-through)
console_run(conn, line, node, confirmation)          -> Reply     (bounded lines)
```

## 11. Decisions

### Decided

**Frontend: SolidJS** (decided 2026-10-25). Rationale:

- The hardest frontend problem is a virtualised tree whose counts change several
  times per second during a scan. Solid's fine-grained reactivity binds each
  count to one text node, so a delta from the backend touches one DOM node.
  No virtual DOM diff, no memoization discipline.
- Stores with path-level writes (`setTree(path, "count", n)`) map directly onto
  the incremental index events the backend emits.
- No VDOM copy of the UI and components that run once mean lower allocation and
  GC pressure while scrolling, which protects the < 150 MB webview budget.
- Less framework work per frame helps most on WebKitGTK (Linux), the slowest of
  the three system webviews Tauri runs on, where the 100 ms first-paint goal is
  hardest.
- Effects without dependency arrays make long-lived Tauri event subscriptions
  simple and free of stale closures.
- JSX + TypeScript, official Tauri template exists.

Accepted trade-offs: smaller component ecosystem (use headless Kobalte or Ark
UI, build the tree ourselves, which folding requires anyway), reactivity
gotchas (no prop destructuring, `<Show>` for conditionals), smaller hiring pool.
Check Solid 2.0 status before the first commit; 1.x is sufficient.

Conventions:

- Tree and list models live in `createStore`; the backend pushes deltas, never
  full snapshots.
- Virtualised rows use `<Index>` (row recycling), windowing via the TanStack
  Virtual Solid adapter.
- Backend windows are fetched with `createResource`, keyed on (path, offset,
  sort), cancelled on scroll by dropping the resource.

**Client crate: `redis` (redis-rs)** (decided 2026-10-25). Rationale:

- Both `redis` and `fred` are protocol-level clients and work equally with
  Redis and Valkey. Server parity is handled in our own capability layer
  (section 10.1), not by the crate, so it did not decide between them.
- What the backend needs from a crate is narrow: async single-node connections,
  pipelining, TLS, RESP3 value types including push messages, and a raw command
  builder. The scan engine and the parity requirements mean we open per-node
  connections and route by slot ourselves, so a crate's cluster abstraction is
  unused. That removes `fred`'s main advantage.
- The raw `cmd("SCAN").arg(..)` style suits a tool whose console must send
  arbitrary commands and whose type registry dispatches on strings.
- Widest usage in the Rust ecosystem, so edge cases in RESP3 parsing and TLS
  have been hit by others first.

Accepted trade-offs: Redis Ltd staff are among the maintainers, so Redis-first
features may land before Valkey ones; this is low risk because the protocol is
neutral and we do not rely on crate-level feature detection. Valkey GLIDE is
revisited only if it ships a stable public Rust crate.

Conventions:

- Cluster and sentinel topology handling lives in our connection manager, not
  in the crate's `ClusterClient`.
- Feature availability comes from the capability layer only. No code path reads
  `redis_version`.
- Every command the browser issues is a raw `Cmd`; typed helper methods from the
  crate are not used, so the command list for ACL documentation stays complete
  and greppable.

**Trigram FTS: opt-in per connection, default off** (decided 2026-10-25).
Rationale:

- On small databases the row scan is already instant, so a default-on index
  would cost disk and scan time for nothing.
- On large databases the user should make the trade (2 to 3 times the index
  size, slower scans) knowingly, and the moment a substring search feels slow
  is the right moment to offer it.
- The index can be built later from `keys` without rescanning the server, so
  deferring the decision costs nothing.

Conventions:

- Setting lives on the connection profile (`substring_index: bool`).
- The search result header offers to enable it after the first substring query
  over 500 ms; enabling builds the FTS table in the background with progress.
- Disabling drops the table immediately and reclaims disk with an incremental
  vacuum.
- Prefix-shaped and glob queries never use the FTS table, regardless of the
  setting.

**Folding heuristics: shape classes counted at ingestion, structural
similarity lazily, user overrides on top** (decided 2026-10-25). Full design in
section 5.6. Rationale:

- Counting shape classes per parent during ingestion makes the fold decision an
  O(1) lookup at expansion time and avoids any sampling bias.
- Treating mixed levels (ids plus a few literals) as the normal case matches
  real keyspaces.
- Hysteresis and decide-at-expansion keep the tree stable during a running
  scan.
- Overrides keyed by path pattern let users fix the rare misclassification
  without us chasing every naming convention.

Conventions:

- Thresholds are global settings with the defaults above; path rules live on
  the connection profile.
- Fold labels use the class names from the table; renamed levels keep the
  user's name.
- The flat list and search surfaces reuse the same fold decision when they
  gain folding later.

**Storage: indexes in the app cache dir per connection id, profiles in app
data, credentials in the OS keychain** (decided 2026-10-25). Full rules in
section 3.4. Rationale:

- The original question (app data dir vs. next to the profile) was a false
  choice: profiles live in app data too. The real distinction is data vs.
  cache, and the index is a cache by the document's own principle.
- Cache directories get backup exclusion and cleanup-tool semantics for free;
  purging costs only a rescan.
- Key names are sensitive enough to need user-only permissions and an
  ephemeral option, defaulting on for prod.
- A configurable cache root and a schema version are cheap now and painful to
  retrofit.

**Command console: a bottom panel, request/reply only, on its own
connection** (decided 2026-11-22). Rules in section 8. Rationale:

- A client without a way to run a command is not used daily, however good
  its browser is; section 4 already reserved "a separate tool" for what the
  browser itself never sends.
- A typed `SELECT`, `MULTI` or `CLIENT REPLY OFF` on a connection the
  browser shares would corrupt the index or desynchronise replies. A
  dedicated connection makes that impossible instead of unlikely.
- Refusing stateful, blocking and streaming commands keeps the first
  version a pure request/reply tool. Transactions, `SELECT` with tracked
  state, `MONITOR` and pub/sub are separate tools to design later.
- The ACL documentation stays truthful: the browser's own commands are the
  listed ones, the console sends the user's commands under the user's ACL,
  through one builder that says so.
- Reading the touched keys again after a write keeps "the tree never shows a
  key the user just deleted" true for the console as well.

Conventions:

- The console's helpers (`COMMAND INFO`, `COMMAND GETKEYS`) are ordinary
  listed browser commands.
- On a cluster a command with keys goes to the owner of their slot, and the
  keys must share one; a command without keys goes to the node the user
  chose. There is no fan-out.
- History is kept in memory for the session only; typed lines can hold
  values.

**SSH tunnels: in-process, forwarding on demand** (decided 2026-11-22).
Rationale:

- Many production servers are reachable only from a bastion; without a
  tunnel the tool cannot be pointed at the databases it was built for.
- Cluster nodes, redirect targets and replicas are addresses the server
  announces, usually private ones. One fixed forwarded port would work for a
  standalone primary and nothing else, so the tunnel forwards any host and
  port on demand and names are resolved on the bastion.
- An SSH client inside the app behaves the same on Linux, macOS and Windows
  with nothing installed, and lets the app ask for a passphrase and show a
  host key. The cost is accepted: `~/.ssh/config` and `ProxyJump` are not
  read in the first version.

Conventions:

- Every connection of a profile goes through the same tunnel, at the one
  place where the connection manager connects; the logical node address
  stays the announced one.
- The tunnel is a property of a stored connection. Quick connect by URL has
  none.

### Still open

None. All concept-level decisions are closed; remaining choices are
implementation details to be settled in the first build slices.

## 12. Suggested build order

1. Rust backend: connection manager + capability layer + `SCAN` engine +
   SQLite index with `keys` and `nodes`. CLI harness that prints progress and
   node counts. Benchmark against a generated 10M-key db. CI matrix with
   Redis 7.2, Redis 8.x, Valkey 8.x, Valkey 9.x from day one.
2. Tauri shell (Solid template) with flat virtualised list over `list_rows`.
   Proves IPC windowing and the webview memory bound.
3. Tree over `tree_children`, live counts during scan, persistence across restart.
4. Folding of high-cardinality levels (section 5.6): shape counters first,
   then aggregated structure, buckets, overrides.
5. Value panel with paged collection reads.
6. Search (glob, substring, regex), jump-to-key.
7. Write-through mutations, pattern delete, safety confirmations.
8. Cluster, replica preference, gentleness profiles, keyspace notifications.
9. Command console (amended 2026-11-22): tokenizer, classification, routing
   and index follow-up in the backend and the CLI first, then the panel.
10. SSH tunnels (amended 2026-11-22): the tunnel and a test bastion in the
    backend and the CLI first, then stored profiles, secrets and the editor.
