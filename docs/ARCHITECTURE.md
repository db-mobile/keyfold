# Architecture and implementation notes

How Keyfold is built, slice by slice, with the measurements taken along the
way. The design itself is in [`CONCEPT.md`](../CONCEPT.md); open items are in
[`backlog.md`](backlog.md). Paths in this file are relative to the repository
root.

The work followed the build order in `CONCEPT.md` section 12. Step one is
the Rust backend: connection manager, capability layer, `SCAN` engine and
SQLite keyspace index, driven by a CLI harness. Step two is the Tauri 2
shell with a SolidJS frontend showing the flat, virtualised key list over
windows of at most 200 rows. Step three adds the namespace tree with live
counts during a scan and the persisted tree on reconnect. Step four folds
high-cardinality levels. Step five is the value panel with paged collection
reads. Step six is search (glob, substring, regex) and jump-to-key. Step
seven is writes: write-through mutations, pattern delete and the safety
confirmations. Step eight is deployment shape: cluster support with the
backend routing by slot, replica preference for scans, the gentleness
profiles, and a live index from keyspace notifications.

The working name is Keyfold, after the folding of high-cardinality key
levels; crates, binary and app identifier (`dev.keyfold.app`) follow it.

## Layout

| Path | What |
|---|---|
| `crates/core` | library: `connection`, `tunnel`, `capability`, `commands`, `cluster`, `route`, `scan`, `browse`, `console`, `index`, `segment`, `fold`, `value`, `search`, `glob`, `mutate`, `notify` |
| `crates/cli` | `keyfold` binary: `info`, `scan`, `keys`, `exec`, `tree`, `fold`, `reindex`, `search`, `fts`, `delete`, `watch`, `gen`, `bench-ingest` |
| `src-tauri` | Tauri shell: stored connections (`profiles`, `secrets`), sessions, scan forwarding, windowed commands (`list_rows`, `tree_children`, `key_meta`, `value_*`, `search*`), writes (`mutate`, `delete_*`, `flush`), scan options, live updates, memory stats |
| `src`, `index.html`, `vite.config.ts` | SolidJS frontend: connection manager, header with scan progress per node, profile and target selectors, live toggle, virtualised tree, list, value panel with editors, search, dialogs |
| `docs/commands.md` | every server command the backend sends, reads and writes, for ACL authors (checked by a test) |
| `docs/backlog.md` | open items from steps one to eight: concept findings to decide, missing features, deferred work |
| `scripts/servers.sh` | starts a primary, a replica of it and a six-node cluster for one image, on the host network |
| `scripts/matrix.sh` | runs the test suite against Redis 7.2, Redis 8, Valkey 8 and Valkey 9 in Docker, each with replica and cluster |
| `scripts/bench.sh` | generates a 10M-key database and times a cold scan, a rescan and a tree window |
| `assets/icons/` | the app icon: `icon.svg` is the master, every other file there is generated from it |
| `scripts/regenerate-icons.sh` | regenerates the icon bitmaps, `icon.icns` and `icon.ico` from `assets/icons/icon.svg` (`npm run icons:regen`); needs `rsvg-convert` |
| `scripts/build-tauri.sh` | builds the release bundles listed in `tauri.conf.json`, the AppImage today (`npm run build:tauri`); puts GNU `cp` first where `cp` is the Rust coreutils, which linuxdeploy's GTK plugin cannot use |
| `.github/workflows/ci.yml` | fmt, clippy, and the four-server test matrix |

## Try it

```sh
docker run -d --rm --name kv -p 6379:6379 valkey/valkey:9
cargo run --release --bin keyfold -- info redis://127.0.0.1:6379/0
cargo run --release --bin keyfold -- gen redis://127.0.0.1:6379/0 --keys 1000000 --flush
cargo run --release --bin keyfold -- scan redis://127.0.0.1:6379/0
cargo run --release --bin keyfold -- tree .local/index/127.0.0.1_6379_0.sqlite "user:" --sort numeric --limit 20
cargo run --release --bin keyfold -- search .local/index/127.0.0.1_6379_0.sqlite "user:42:*" --count
cargo run --release --bin keyfold -- delete .local/index/127.0.0.1_6379_0.sqlite "cache:2024-01-*" --url redis://127.0.0.1:6379/0   # preview; add --yes
cargo run --release --bin keyfold -- watch redis://127.0.0.1:6379/0 --enable --seconds 30                     # keyspace notifications
scripts/servers.sh start valkey/valkey:9                                                         # + replica + cluster
cargo run --release --bin keyfold -- info redis://127.0.0.1:16400/0                                            # topology
cargo run --release --bin keyfold -- scan redis://127.0.0.1:16400/0 --scan-target replica                      # one cursor per shard
```

`scan` prints live progress (exact against `DBSIZE`, adaptive `COUNT`, round
trips, queue depth) and ends with timings, index size, process RSS and the
root of the namespace tree with exact counts and shape classes. Stop it with
Ctrl-C or `--stop-after-calls N` and run it again: the cursor is persisted
with every merge and the scan resumes. `--match 'user:*'` reads a pattern
into the index and never prunes. `--subtree user:` scans one subtree with a
freshness record of its own: exact under the prefix afterwards, the rest of
the index and the full scan's state untouched. `--sample 20000` stops after
about that many keys and establishes nothing; `tree` says for a prefix
whether a completed scan covers it. `keys <url>` pages through plain `SCAN`
with no index at all. Prod-tagged connections (`--tag prod`) default to the
polite profile and an in-memory index.

Live-server tests need `KEYFOLD_TEST_URL` (the database is flushed);
the replica and cluster tests also need `KEYFOLD_TEST_REPLICA=1` and
`KEYFOLD_TEST_CLUSTER_URL`, which `scripts/servers.sh` prints:

```sh
KEYFOLD_TEST_URL=redis://127.0.0.1:6379/15 cargo test --workspace
scripts/servers.sh start redis:8 && env $(scripts/servers.sh env | xargs) cargo test --workspace
scripts/matrix.sh          # all four servers, each with replica and cluster, needs Docker
```

## Desktop shell

```sh
npm install
npm run tauri dev            # or: cargo tauri dev
KEYFOLD_AUTOCONNECT=redis://127.0.0.1:6379/0 npm run tauri dev   # connect on launch
npm run tauri build          # release bundle with the frontend embedded
cargo build --release -p keyfold-app --features custom-protocol   # same binary, no bundling
```

Without the `custom-protocol` feature a release binary still loads the Vite
dev server; that feature is what `tauri build` turns on. Frontend errors and
`window.onerror` go to the backend log through the `app_log` command, so a
headless run with `RUST_LOG=keyfold_app_lib=debug,webview=debug` shows
page loads, connects, window requests and metadata fetches with timings.

On connect the shell probes the server, opens (or creates) the index under
the app cache directory (`indexes/<host_port_db>/db<N>.sqlite` plus node
shards; a temp directory for prod-tagged connections, deleted on
disconnect) and starts what the connection's index mode starts with, on its
own connection. The header shows server identity, key count and memory, a
progress bar fed by `scan:progress` events at four updates a second, and the
backend and webview RSS as read from `/proc`.

The start screen is the connection manager. Stored connections live in
`profiles.json` in the app data dir (`~/.local/share/dev.keyfold.app` on
Linux), each with a UUID that names its index folder in the cache dir
(`indexes/<uuid>/db<N>.sqlite`), so renaming one keeps its index. The
password is never in that file: it goes to the OS keychain (Keychain
Services, Credential Manager, Secret Service) under the service
`dev.keyfold.app`, and the webview can set it but not read it back. The
list shows each connection's index size and offers connect, edit, "clear
index" and delete (which also removes the keychain entry and the index
folder). The editor takes the fields one by one or fills them from a
pasted URL. Scan profile and target changed in the header are written
back to the stored connection. "Quick connect by URL" stores nothing.
`KEYFOLD_AUTOPROFILE=<url>` stores, connects, lists and deletes a
connection, for smoke tests.

Everything the app writes is the user's alone on Unix
(`crates/core/src/private.rs`): mode 0700 on the data directory, the
`indexes` folder and each index folder, 0600 on index files (set before
SQLite writes to them, so WAL and SHM files inherit it), `profiles.json`
and `known_hosts`. The index of a prod-tagged or index-less session is in a
directory of its own with a random name under the temp directory.

The webview runs under a content security policy
(`src-tauri/tauri.conf.json`): scripts and styles from the app itself only,
images from the app or `data:` (the icons Vite inlines), connections to the
IPC endpoint only. `devCsp` is the same plus inline styles and the Vite
websocket. Styles set from code go through the CSSOM (Solid's object
`style`), which the policy does not restrict.

More smoke-test aids: `KEYFOLD_AUTOSORT=<name|count|numeric>` changes the
tree sort after `KEYFOLD_AUTOEXPAND` and logs the row count before and
after; every load of the value panel is logged with its reason (`selected`,
`live`, `reload button`), so `KEYFOLD_AUTOSELECT` with `KEYFOLD_AUTOLIVE=1`
shows how often a changing key is reloaded.

The `db` selector in the header switches the logical database. It lists
databases `0..N` (`CONFIG GET databases`, or `cluster-databases` on a
cluster; 16 are assumed when `CONFIG GET` is denied) with the key counts
from `INFO keyspace`. A switch opens a new session on the same server with
the tag, credentials and scan options of the old one, and its own index
file, so going back to a database that was scanned before shows its tree
at once. When the server refuses the database the open one stays.
`KEYFOLD_AUTOSWITCHDB=<db>` does one switch after the first scan, for
smoke tests.

The list is a TanStack virtualizer over row numbers. The backend serves
`list_rows(offset, limit)` through *anchors*: every 4096th key name, sampled
once per scan generation (about a second for 10M keys, refreshed in the
background while a scan grows the index), so any offset becomes a keyset
query that skips at most 4096 rows. The webview caches at most 32 windows,
requests windows only once scrolling has settled for 80 ms, and asks for
`TYPE`/`TTL`/`MEMORY USAGE` of the rows on screen 150 ms after that, in one
pipeline. Row numbers are exact after a scan completes; during a scan the
footer shows "≥".

Measured with the release binary against a 1M-key Valkey 9 in Docker
(2026-10-25): connect and probe 180 ms, first window of 200 rows on screen
270 ms later (the first merge of 10k keys lands and the list picks it up on
the next progress event), windows served in 0.5 to 5 ms, metadata for the
42 visible rows in 0.6 ms, the whole 1M-key scan 3.5 s. Webview RSS was
204 MB with the list on screen against a target of 150 MB; the same binary
sitting on the empty connect form already uses 159 MB, so on WebKitGTK the
WebKit process baseline exceeds the target before any row exists and the
list itself costs about 45 MB. The backend process (Tauri host plus GTK)
starts at 195 MB and sat at 331 MB right after the scan.

### Index modes

A connection is in one of three index modes (CONCEPT 7). The profile names
one, or leaves it to the size: everything up to 20 million keys
(`AUTO_INDEX_THRESHOLD`), on demand above. The mode is chosen in the
connection editor, in quick connect and in the scan options of the footer;
a change there replaces the session like a database switch
(`set_index_mode`) and is written back to a stored profile.

| Mode | On connect | Tree, list, search | Scans |
|---|---|---|---|
| `full` | the full scan, or its resume | exact once the scan completed | "Rescan"; "Rescan this subtree" in the tree menu |
| `onDemand` | a sample of about 20,000 keys when the index is empty | what was sampled and scanned; counts are lower bounds outside scanned subtrees | expanding an unscanned node scans its subtree (prod asks first); "Scan everything" after a confirmation |
| `none` | nothing | not offered; the sidebar is a list of `SCAN` pages | none |

**Freshness per subtree** (`crates/core/src/index/coverage.rs`). A subtree
scan is `SCAN MATCH prefix*` with the prefix escaped (`Query::prefix`), so
the server walks its whole table and only matches cross the network. Its
record is a row of the `scans` table: the generation and time of the last
completed scan of that prefix, and the cursors of one in progress, per node.
The full scan's state in `meta` is not touched, so an interrupted full scan
resumes after any number of subtree scans; generations come from one counter
(`scan.generation_seq`) so that a later scan is always newer. Completing a
subtree scan prunes what it did not see within `[prefix, prefix_end)` and
nowhere else, and drops the records of prefixes beneath it; completing a
full scan drops them all. `Index::coverage(prefix)` answers with the newest
completed scan of the prefix, one of its ancestors, or everything. A prefix
without one is *uncovered*: its keys are those a sample, a notification or a
write through the app happened to bring, and its counts are lower bounds.

The shell turns that into `exact` on `tree_root`, `tree_children` and
`list_rows` (covered, and no scan running over it), and into `scannedAt` and
`scan` (`idle`, `queued`, `running`) on every tree node. One scan runs at a
time; `scan_subtree` queues behind it, skips a prefix a running or queued
scan already includes, and `scan_cancel` stops the running one and empties
the queue. A filtered scan cannot count what the server walked, so its
progress is estimated from the `COUNT` it asked for and marked as such.

**Without an index** (`crates/core/src/browse.rs`, `keys_page`). A `Pager`
walks the nodes a scan would read (replicas, with the profile's gap between
calls) and hands out at most 200 keys a page; what a reply returns beyond
that waits for the next page. The search box becomes the `MATCH` pattern (a
glob, or a substring as `*text*`; a regular expression needs the index). A
page is also bounded by 40 calls and one second, so with a pattern it can
come back short or empty with more to walk; the list says so and the next
page is read only on "Load more". The index-backed commands answer with an
error in this mode, live updates are refused, and the session's index files
are empty files in the temp directory that nothing writes to. Opening a key
by name, editing it, creating one and flushing work as everywhere.

### Command console

A panel above the status bar (`` Ctrl+` ``, or the main menu) where a command is
typed and its reply read (CONCEPT 8). The path of one line, all of it in
`crates/core/src/console.rs` and drivable as `keyfold exec <url> -- <command>`:

1. `split_line` reads it like `redis-cli`: quotes, `\xHH` and the usual
   escapes, so a binary argument can be typed.
2. `Console::inspect` asks the server what the command is (`COMMAND INFO`,
   cached per command) and `classify` decides. Refused, by name and whatever
   the server says: commands that change connection state (`SELECT`, `MULTI`,
   `AUTH`, `HELLO`, `RESET`, `CLIENT REPLY`, ...), streams (`SUBSCRIBE`,
   `MONITOR`), and blocking commands (the server's `blocking` flag; `XREAD`
   only with `BLOCK`). Confirmed by typing the connection's name: the flush
   commands, `SHUTDOWN`, `SWAPDB`, `DEBUG`. Confirmed with a warning: `KEYS`,
   and what the server lists as `@dangerous` and can change something
   (`CONFIG SET`, not `CONFIG GET` or `INFO`). On a prod-tagged connection
   everything that may write. A script counts as a write unless it is a
   read-only variant; a command the server does not describe counts as one
   too.
3. `Console::run` sends it on the console's own router, never the
   browser's. On a cluster the keys (`COMMAND GETKEYS`) must share a slot,
   which is said before sending; a command without keys goes to the node
   chosen in the panel.
4. `render` turns the reply into `redis-cli` style lines over every RESP3
   shape, cut to 200 elements a level, 2,000 lines and 4 KB a string, every
   cut marked. The panel keeps 5,000 lines.
5. The index follows (`src-tauri/src/console.rs`): the keys a write named
   are read again (`reconcile`, on `meta::fetch_routed`) and written through
   like any other mutation; a flush clears the index and starts over; a
   write that names no keys sets a "may be stale" banner that a completed
   scan of everything clears.

`console_run` enforces the confirmations: a line that needs one comes back
unsent, and only a second call carrying the confirmation sends it. Without
`COMMAND` in the user's ACL the console still works by name. History is
kept in memory only.

### SSH tunnels

A stored connection can name an SSH bastion (CONCEPT 8 and 11;
`crates/core/src/tunnel.rs`, on the `russh` crate with the `ring` backend
the TLS stack already brings). Every connect of the profile goes through
`ConnectionManager::connect_inner`, and there the tunnel is attached as the
client crate's resolver: for each connection it opens a `direct-tcpip`
channel to the real `host:port` (the name is resolved on the bastion),
binds a port on 127.0.0.1 that accepts exactly one connection, and hands
that port to the client crate. TLS still runs against the real host name,
the router still keys its connections by the announced address, and the
forward is closed to other local processes after its one connection.
Cluster nodes, redirect targets and replicas are reached the same way, so
scans, the console, pattern deletes and paging all work through it.

The dedicated pub/sub client of the crate does not take a resolver, so live
updates through a tunnel subscribe on a RESP3 connection with push
messages instead (`ConnectionManager::subscribe_pattern`).

One SSH login serves every connection of a session and is handed to the
session that replaces it on a database or index-mode switch; a lost SSH
session is logged in again on the next connect. Login is by the SSH agent,
a key file or a password; the password or passphrase is a second keychain
entry (`<profile id>:ssh`). The host key is checked against
`~/.ssh/known_hosts` and the app's own `known_hosts` in the app data
directory. An unknown key stops the connect, which reports it to the
webview (`ssh-host-key-unknown`); the user reads the fingerprint and
`ssh_trust` records it in the app's file. A key that differs from one on
record is refused and never offered.

`scripts/servers.sh` starts a bastion (`scripts/sshd`) and a server only
the bastion can name; `crates/core/tests/tunnel.rs` covers the first
contact, the three logins and their failures, a changed host key, live
updates, and a cluster reached node by node.

### Tree

The left pane is the namespace tree from the `nodes` table, flattened into
one virtualised list. Expanding a node loads one window of 200 children
(`tree_children`, keyset paged, sorted by count, name or numeric value);
scrolling past the last loaded child loads the next window, up to 10,000
rows per node until folding (step four) takes over. Each row shows its
subtree count, as "≥" while a scan runs. While the scan runs, the branches
that have rows on screen are refetched once a second and the counts are
written into the Solid store field by field, so only changed numbers touch
the DOM; the same happens once when the scan ends. The tree and the list
share one selection (a clicked leaf or row shows in the status line).

Reconnecting to a database with a complete index shows the previous tree
and list at once, with the header reading "stale (scanned 3 min ago),
refreshing" while the new generation scans in the background; counts keep
their old values until the generation completes and prunes vanished keys.

Key names are byte strings, JSON strings are not (`src-tauri/src/wire.rs`).
A name, segment or prefix that is valid UTF-8 crosses the IPC as text
alone. Any other crosses as an escaped text for display (valid runs as they
are, other bytes as `\xNN`, a literal backslash doubled) plus the exact
bytes in hex (`raw`, `prefixRaw`), and the webview hands the hex back
unchanged. Everything that names a key works on that pair, `KeyRef` in
`src/api.ts`: the list, tree and search rows, the selection, `key_meta`,
the `value_*` commands and `mutate`. A hash field or a set or sorted-set
member that already exists is addressed the same way (`fieldRaw`,
`memberRaw`). A delete started on a tree node sends the node's prefix as
such a pair and the backend builds the pattern from the bytes
(`Query::prefix`), so glob characters in a key name match only themselves.
What the user types (search box, jump palette, new key, rename target) is
text and is taken literally.

### Folding

A level with at least 100 distinct children is not listed but decided
(`tree_fold`, CONCEPT 5.6). The decision uses the per-class counters the
index maintains at ingestion: a generated class with at least 100 members
covering 90 % of the children folds; several generated classes covering
90 % together fold side by side; a level of 2,000 or more children without
a dominant shape folds as `{*}` when the average Jaccard overlap of the
child sets beneath 200 sampled children is at least 0.6. Everything else
is a plain list. The decision is held until the node is collapsed.

A fold row shows the label (`{n}`, `{unix-ts}`, `{hex32}`, `{uuid}`,
`{id}`, `{*}`), the numeric range, three example ids, the member count and
the evidence as its tooltip. Opened, it lists the aggregated structure
beneath the members with coverage (`profile 100 %`, `cart 14.8 %`), the
buckets (decades for numeric ids, leading character for hex, uuid, ulid and
opaque ids), a "show as list" row that expands the ids as a plain list
sorted numerically where applicable, and a filter box that narrows the ids
by prefix with a live count. Literal siblings (`user:count`) stay as
ordinary rows next to the folds. Bucket and list rows open ordinary
windowed branches with a filter, so they page like any other level.

The aggregated structure is computed exactly by reading every `nodes` row
one level beneath the members (a range scan of the subtree's second level)
and cached per session until the subtree count drifts by more than 5 %.
Measured on the 1M-key index with `keyfold fold <index> user:` (350,000
numeric ids): decision 0.1 ms, summary 220 ms, buckets 100 ms, literal
siblings 20 ms; the hex level `order:` (200,000 ids) takes 740 ms for the
literal scan because only the numeric class can be excluded in SQL.
Per-session rules can force or forbid a fold (`tree_fold_rule`); rules on
the connection profile, renamed levels, custom id classes, affix stripping
and regex filters are not implemented yet, nor is the `fold_stats` table
used as the persistent cache.

### Values

Selecting a key (in the tree or the list) runs one pipeline: `TYPE`,
`TTL`, `MEMORY USAGE`, `OBJECT ENCODING` and the length command of every
native type with errors ignored, so the right length comes back in the
same round trip. Nothing is fetched on hover. A key whose `TYPE` is `none`
shows as gone and is removed from the index at once when no scan owns the
writer (verify on open).

A type registry maps the exact `TYPE` string to a viewer. Hashes, sets and
sorted sets page with `HSCAN`/`SSCAN`/`ZSCAN` (`MATCH` for the filter box);
sorted sets can also be read by rank with scores (`ZRANGE ... WITHSCORES`),
lists by index (`LRANGE`), streams by id (`XRANGE ... COUNT`). Pages hold
200 entries and the next one loads when the list is scrolled to its end;
the header reads "showing 200 of 4,113,920 fields". Strings load a 64 KB
preview over the IPC as raw bytes (`tauri::ipc::Response`, no JSON), the
content kind is decided on the preview (JSON, text, gzip, msgpack, binary)
and picks the viewer: pretty-printed JSON when complete and under 1 MB,
virtualised text lines, or a virtualised hex dump. "Load full" streams the
rest in 1 MB chunks up to 32 MB. Types without a viewer (module types)
fall back to a raw view with the `DUMP` size and a hex peek.

### Search and jump to key

One search box above the key pane; the backend classifies the input by its
syntax and the pane shows the mode as a chip (CONCEPT 5.3):

| Input | Mode | How it runs |
|---|---|---|
| `user:42:*` | glob | B-tree range on the literal prefix, a `LIKE` superset of the glob in SQL, exact glob match in Rust (the server's own `MATCH` rules, `glob.rs`) |
| `session` | substring | `LIKE '%session%'` over the range, ASCII case-insensitive; or the trigram table when the index has one |
| `/user:\d+:cart/` | regex | streamed through the index in windows of 5000 candidates, `^literal` narrows the range |
| `user:42:profile` + Enter | jump | opens the key directly through the value panel's verify-on-open; no index involved |

Results arrive in windows of 200 in byte order, keyed on the last row; the
pane loads the next window when scrolled to its end and stops at 10,000
rows ("refine the query"). After the first window the exact count runs as a
second query. A newer query cancels the running one: every search has a
flag the Rust loops poll and the search reader's SQLite interrupt handle,
so a one-second substring scan stops within a window. Searches run on their
own read-only index, so list and tree windows never wait behind one.

"Verify against server" (offered for globs and substrings while no scan
runs) walks the server with `SCAN MATCH`, bounded by calls, time and match
count, and reconciles that pattern: keys the server has and the index lacks
are added, keys the index has and the server did not return are removed,
the latter only when the walk completed. The persisted scan cursor is left
alone, so an interrupted scan still resumes.

The trigram table (CONCEPT 3.3, opt-in, default off) is offered in the
result header after a substring search slower than 500 ms. Enabling builds
an FTS5 `trigram` table from the keys already in the index, in the
background with progress events and no server traffic; each name's FTS5
rowid is recorded on its `keys` row (`fts_rowid`, schema version 3), so a
delete is a point lookup, and ingestion keeps the table in step while it
exists. Disabling drops it and runs `VACUUM`. Queries under three
characters and prefix-shaped globs never use it.

Ctrl+K opens the jump palette: an exact key name, Enter opens it; the
twenty most recently opened keys of the connection are listed first (kept
in the webview's local storage, a few names, never the keyspace).

The CLI has the same paths without a window: `keyfold search <index>
<query> [--count] [--verify <url>]` and `keyfold fts <index> [--drop]`.

### Writes and safety

Every write (CONCEPT section 7) is one `mutate` command: the operation
runs on the server in a pipeline that ends with `EXISTS` and `TTL` of the
affected key, and the backend brings the index in line in the same action
(`mutate::write_through`): a deleted or renamed-away key leaves `keys` and
the tree counts, a created or renamed-to key is upserted with its expiry,
a collection that lost its last member is dropped. While a scan owns the
index writer the index update is queued and applied the moment the writer
comes back (the result says `queued`); the value panel reloads the key
either way, and an `index:changed` event makes the tree, list and search
panes refetch what they show.

| Where | Writes |
|---|---|
| Value panel header | rename (`RENAME`), ttl (`EXPIRE` / `PERSIST`), delete (`UNLINK`, `DEL` when missing), edit a text or JSON string up to 1 MB (`SET KEEPTTL`) |
| Collection rows | add and remove fields, members, items and entries (`HSET`/`HDEL`, `SADD`/`SREM`, `ZADD`/`ZREM`, `LPUSH`/`RPUSH`/`LSET`/`LREM`, `XADD`/`XDEL`); hash values, scores and list items edit in place |
| Key pane toolbar | "+ key" creates a key of any native type with its first field, member, item or entry |
| Search header | "delete all N…" for the current query (glob, substring or regex) |
| Tree node, right click | search the prefix, open the key, delete everything under the prefix |
| Header | "Flush…": `FLUSHDB` or `FLUSHALL` |

Confirmations (CONCEPT section 8) are enforced in the backend, not only
in the dialogs: a prod-tagged connection refuses any write that was not
confirmed (`confirmed: false`), a pattern delete always needs one, and a
flush needs the connection's name typed exactly. Deleting a key confirms
on every tag; field-level edits confirm on prod only.

A pattern delete goes through the index: the dialog shows the exact count
and the first 20 names, then `delete_start` takes the index writer and a
connection of its own and runs `UNLINK` in batches of 500 names read
from the index in byte order; each batch is removed from the index only
after the server confirmed it, so pause, resume and stop (progress events
four times a second) always leave the server and the index in step. Keys
the server has and the index does not are not touched; "verify against
server" in the search header reconciles first when that matters. Prod
connections wait the polite profile's gap between calls.

Removing a list item by index is the usual `LSET` to a sentinel followed
by `LREM`, two commands in one pipeline. Removing the last field, member
or item of a collection deletes the key on the server; the panel shows it
as gone and the index drops it.

The CLI runs the same engine: `keyfold delete <index> <query> --url <url>`
prints the count and sample, `--yes` deletes with progress.

Measured with the release build against the 1M-key Valkey 9 in Docker
(2026-10-26): a single mutation (`SET`, `EXPIRE`, `HSET`, `HDEL`, `RENAME`,
`UNLINK`) including its index update takes 1.3 to 2.6 ms; a pattern delete
of 8,334 keys (`cache:2024-06-*`) takes 176 ms in 17 calls through the app,
190 ms through the CLI with the trigram table present. The first version
of that delete took 49.5 s with the trigram table: deletes looked the FTS5
rowid up with a `GLOB` through the trigram index, about 6 ms per name and
growing as deleted rows piled up in the posting lists. Hashing the name
into the rowid made deletes free but tripled the table (FTS5 stores rowid
deltas as varints, and random 64-bit rowids are eight bytes apart), which
is why the rowid is now stored on the key's row instead: sequential rowids,
one-byte deltas, and the `DELETE ... RETURNING fts_rowid` of the key row
hands the trigram row to delete. Building the table for 1M keys now takes
6.2 s (4.8 s before the write-back) and the index grows from 138 MB to
226 MB, as before.

### Cluster, replicas, gentleness and live updates

The backend owns the topology (CONCEPT 10.1): on connect to a node in
cluster mode it reads `CLUSTER SHARDS` (or `CLUSTER SLOTS`) and keeps a
16384-entry slot map (`core::cluster`). `core::route::Router` opens one
connection per node on first use and routes every key by its `CRC16` slot;
a `MOVED` reply corrects the map and refreshes the topology, an `ASK`
sends `ASKING` on the named node once, a transport error reopens the
connection once (`routed!`). A standalone server is a router with one
node, so the value, metadata, mutation, delete, verify and flush paths do
not branch on the deployment kind. Pipelines (`key_meta`, pattern delete,
the generator) are grouped per owning node; on a cluster a delete batch is
one pipeline of single-key `UNLINK` calls per node, because multi-key
commands across slots are refused with `CROSSSLOT`. The same rule makes a
rename across slots impossible, which the shell says before sending it.
`DBSIZE` and `FLUSHDB` cover every primary.

The scan engine runs one `SCAN` task per node into one index. Each
node's cursor is persisted with every merge (`scan.cursor` holds
`id=cursor;id=cursor`), a stop leaves all of them resumable, and a resume
requires the same node set, shape and generation. Progress is the sum
with a per-node breakdown that the header shows ("reading 3 replicas: a
33%, b 34%, c 33%"). `scan::targets` chooses the nodes (CONCEPT 4,
"prefer replicas"): standalone, the replicas `INFO replication` lists are
tried in order and the server itself is the fallback; cluster, one node
per shard, an online replica with `READONLY` when the target allows, else
the primary. The target is `auto`, `primary` or `replica`; the profile is
`fast` (continuous), `normal` (2 ms gaps, backs off) or `polite` (50 ms
gaps, replicas only). Polite without a reachable replica refuses to start
and the header offers "scan the primary anyway", which sets the target to
`primary`. The normal profile samples `INFO stats` and `SLOWLOG LEN` every
2 s on each scanned node (`scan::LoadMonitor`): when the server's own load
(ops per second minus our own calls) rises above 1.5 times the level
measured before the scan plus 100, or the slow log grows, the gap doubles
and the `COUNT` cap halves, up to level 6 (128 ms gaps, `COUNT` 156); three
calm samples step back one level.

Live updates (CONCEPT 7) are opt-in per connection: `core::notify`
subscribes to `__keyevent@<db>__:*` on every primary through the client
crate's subscription stream, batches events every 250 ms (or 2,000
events), reduces each batch to one state per key and the shell applies it
through the same write-through path as a mutation (queued while a scan
owns the writer). `del`, `expired`, `evicted` and `rename_from` remove,
everything else upserts with its cached expiry cleared. The server must
publish key events: the header reads `CONFIG GET notify-keyspace-events`
and, when they are off, asks before running `CONFIG SET` with `E` and `A`
added to the current value on every primary. A dropped subscription is
retried with backoff from 1 s to 30 s and the badge shows "reconnecting".
When all nodes are subscribed and the index is complete the badge reads
"live". The CLI's `watch` prints the events or applies them to an index.

Measured with the release build against Redis 7.2 in Docker (2026-10-26):
a 300k-key cluster of three shards scans through its three replicas in
0.57 s (14 calls per node, `COUNT` 10,000, 530k keys/s); a 200k-key
standalone scans through its replica in 0.49 s with the normal profile and
1.6 s with the polite profile; a routed mutation on the cluster takes 1 to
2 ms including the index update; a 2,000-key pattern delete over three
nodes takes 4 to 12 pipelines; live events reach the index 50 to 250 ms
after the write. `scripts/servers.sh` boots the primary, replica and
six-node cluster of one image in about five seconds.

Not in these steps: sorting the flat list by other columns, filters, caching
the lazily fetched metadata into the index, module type viewers (JSON,
search, time series, bloom), msgpack and gzip decoding, folding search
results (CONCEPT 5.6 "later"), persisting the trigram setting on a profile
(it lives with the index file until profiles exist), verifying regex
results on the server, editing binary values, typing a key name or a
pattern that is not UTF-8,
type conversion of a key, pattern deletes while a scan runs (the scan owns
the writer; the offer is hidden meanwhile), an undo, sentinel discovery,
TLS parameters per node, a node tag on index rows (the owning node follows
from the slot, so it is derived instead of stored), re-routing a running
scan after a failover (the scan fails on that node and a rescan picks the
new topology), and a per-node view of key-event throughput. The tree
refresh is pull based (the webview refetches the windows on screen); the
delta push from the backend named in the concept's conventions is deferred
until the backend tracks what each window has expanded.

## How the index is written

`SCAN` returns keys in hash order. Inserting them one batch at a time into a
B-tree keyed by name makes nearly every row dirty its own page, and SQLite
then writes that page out again for every small transaction: the first
version of this code wrote 149 GB to disk for a 10M-key scan. The engine
therefore buffers keys in an arena (`index::KeyBatch`), sorts them, and
merges them in large sorted transactions. The first merges are small
(10k, 40k, 160k keys) so the tree appears within the first second; later
merges grow to `merge_max_keys`.

SQLite has one writer per file, so the index is one *keys* file plus N node
*shard* files, each row placed by a hash of its path. The scan engine runs
one writer thread per file ([`index::workers`]): the keys file commits a
merge with the scan cursor while the shards upsert that merge's tree rows
and the coordinator already prepares the next batch. Every file records the
merge sequence it last committed; if a crash leaves them apart, `Index::open`
rebuilds the tree from the keys. Reads of one parent's children fan out to
all shards and merge, using keyset pagination with a hard cap of 200 rows.

## Benchmark (10M keys, Redis 8 in Docker, local NVMe, 22 cores)

Generated keyspace: `user:{n}:profile|cart|settings|sessions:{uuid}`,
`order:{hex32}`, `cache:{date}:{n}`, `tenant:{n}:user:{n}:session:{uuid}`,
`event:{unix-ts}`; 10,000,000 distinct keys, 1004 `SCAN` calls at `COUNT`
10000. Numbers from `scripts/bench.sh`-style runs on 2026-10-25:

| Configuration | Cold full scan | Peak RSS | RSS after | Disk written | Index on disk |
|---|---|---|---|---|---|
| 4 shards, merges up to 2M keys, 64 MB cache (default) | 63 to 68 s | ~600 MB | ~170 MB | 21 GB | 1.4 GB |
| 4 shards, merges up to 1M keys, 64 MB cache | 95 s | ~430 MB | ~200 MB | 37 GB | 1.4 GB |

A rescan of the unchanged database (every key already indexed, only
`seen_at` moves) takes 47 s at a peak of 240 MB. Tree windows of 200 rows:
name order reads in under a millisecond at any level (the primary key order);
numeric and count order over a level with 3.5M children take about 300 ms,
because they sort the whole level. The 50 ms target for those two sorts is
left to the folding step, which decides which secondary indexes the `nodes`
table carries.

Targets from `CONCEPT.md` section 9 are < 60 s and < 300 MB RSS during and
after the scan. The scan is a few seconds over the time target and the RSS
target holds after the scan; during the scan the two targets pull against
each other, because every merge rewrites most pages of every file and fewer,
larger merges mean more memory in flight. Levers not yet pulled: build the tree once at the end
of a first scan into an empty index (one rewrite instead of five, at the cost
of live counts during that first scan), a second keys thread stage so key
inserts overlap fully, and tighter entry packing. Tree windows of 200 rows
read in well under a millisecond at any level.

### Search (same 10M-key index, built offline with `bench-ingest`, 2026-10-26)

First window of 200 matches and the exact count, `keyfold search --count`,
page cache cold for the first row:

| Query | Mode | First window | Count | Matches |
|---|---|---|---|---|
| `user:42:*` | glob, prefix range | 53 ms cold, 5 ms warm | 47 ms | 5 |
| `session` | substring, `LIKE` scan | 240 ms | 720 ms | 2,000,000 |
| `cart` | substring, `LIKE` scan | 480 ms | 950 ms | 500,000 |
| `xq7` (no match) | substring, `LIKE` scan | 670 ms | 615 ms | 0 |
| `*:cart` | glob, no prefix | 410 ms | 900 ms | 500,000 |
| `/^user:4\d+:cart$/` | regex, `^` prefix | 1.2 ms | 150 ms | 111,110 |
| `/zzz/` (no match) | regex, full stream | 1.8 s | 1.7 s | 0 |
| `xq7` (no match) with trigram table | substring, FTS | 0.2 ms | 0.1 ms | 0 |
| `4242:c` with trigram table | substring, FTS | 7 ms | 8 ms | 50 |
| `Session:1` with trigram table | substring, FTS | 380 ms | 320 ms | 62,225 |
| `session` with trigram table | substring, planner chose the scan | 480 ms | 1.0 s | 2,000,000 |

Targets (CONCEPT section 9): glob under 50 ms, substring without FTS under
1.5 s and cancellable, substring with FTS under 100 ms. The last holds for
selective terms, which is what the table is for; a term whose rarest
trigram still sits in hundreds of thousands of keys (`Session:1`) costs
about what the scan costs, and a term made of common trigrams (`session`,
20 % of all keys) is routed to the scan by the planner, as the concept
predicts for common trigrams.

The trigram table is `detail = none` (no positions; `LIKE` through the
index needs none) with automatic rowids, each written back to the key's
row for deletes: FTS5 flushes a segment whenever a rowid arrives out of
order, so the first version with hashed rowids took 18 minutes for 10M
keys and 3.4 times the index size. As it stands, building it for 10M keys
takes 132 s (110 s before the rowid write-back) and grows the index from
1.4 GB to 2.3 GB; for 1M keys 6.2 s and 138 MB to 226 MB. Rare terms still
answer through it in under 10 ms at 10M (`xq7` 0.2 ms, `4242:c` 12 ms for
50 matches). Keeping it up to date during
a scan costs more than the concept's 30 to 50 percent: a fresh 1M-key scan
takes 9.2 s instead of 3.3 s, because the keys thread tokenises every new
key in line with the inserts. The lever not pulled: queue new names in a
side table during the scan and bulk-load them afterwards.
