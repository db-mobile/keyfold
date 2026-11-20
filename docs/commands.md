# Commands the backend sends

Every command is built as a raw command in `crates/core/src/commands.rs`.
A test fails when this list and that file disagree. Operators can turn the
browsing list into an ACL rule; the server degrades cleanly when a command
is denied (`NOPERM`).

## Handshake (sent by the client crate on connect)

| Command | Purpose |
|---|---|
| `HELLO` | RESP3 negotiation; RESP2 is used when `HELLO` is rejected |
| `AUTH` | when a username or password is configured |
| `SELECT` | when the logical db is not 0 |
| `CLIENT SETINFO` | library name and version |

## Browsing

| Command | Purpose |
|---|---|
| `PING` | round-trip baseline for the adaptive `SCAN COUNT` |
| `INFO` | server identity, role, memory (sections `server`, `replication`, `memory`); `keyspace` for the key counts in the database selector |
| `CONFIG GET databases` | how many logical databases the selector offers (`cluster-databases` on a cluster); 16 are assumed on a standalone server and one on a cluster when it is denied |
| `DBSIZE` | exact total for scan progress |
| `COMMAND INFO` | capability probing on connect; in the console, what a typed command is (flags and ACL categories) |
| `COMMAND GETKEYS` | console only: which arguments of a typed command are keys, to route it on a cluster and to read those keys again after a write |
| `SCAN` | keyspace enumeration, with `MATCH`, `COUNT`, `TYPE` |
| `TYPE` | lazy type for visible rows |
| `TTL` | lazy TTL for visible rows |
| `MEMORY USAGE` | lazy memory estimate for visible rows |
| `EXISTS` | verify-on-open and index reconciliation |
| `OBJECT ENCODING` | shown when a key is opened |
| `STRLEN`, `GETRANGE` | string length, preview and chunks |
| `HLEN`, `HSCAN` | hash size and pages, field search with `MATCH` |
| `SCARD`, `SSCAN` | set size and pages |
| `ZCARD`, `ZSCAN`, `ZRANGE` | sorted set size, pages, rank windows with scores |
| `LLEN`, `LRANGE` | list size and windows |
| `XLEN`, `XRANGE` | stream size and pages (`COUNT` bounded) |
| `DUMP` | size and raw peek of a type without a viewer |

## Cluster and replicas (CONCEPT 4 and 10.1)

| Command | Purpose |
|---|---|
| `CLUSTER SHARDS` | topology on connect and after a `MOVED` reply; the backend routes by slot itself |
| `CLUSTER SLOTS` | the same when `CLUSTER SHARDS` is missing or denied |
| `READONLY` | sent once on a cluster replica connection that will serve `SCAN` |
| `ASKING` | precedes the retry of a command after an `ASK` redirect |
| `INFO stats` | `instantaneous_ops_per_sec`, sampled every 2 s by the normal scan profile |
| `SLOWLOG LEN` | the normal profile backs off when it grows |

## Live index (CONCEPT 7, keyspace notifications, opt-in)

| Command | Purpose |
|---|---|
| `CONFIG GET notify-keyspace-events` | whether the server publishes key events |
| `CONFIG SET notify-keyspace-events` | only when the user asks the app to turn them on (adds `E` and `A` to the current value) |
| `PSUBSCRIBE __keyevent@<db>__:*` | the subscription, on every primary; sent through the client crate's subscription API |
| `PUNSUBSCRIBE` | when live updates are switched off |

Never used by the browser: `KEYS`, `DEBUG`, `MONITOR`.

## Console (CONCEPT section 8, command console)

The console sends what the user types. These are the user's commands, not
the browser's: they run under the connected user's ACL, on a connection of
their own, through one builder (`commands::console::user`). No list can
name them, and an ACL written from the tables in this document denies
whatever it does not grant, in the console as everywhere (`NOPERM` is shown
as the reply).

What the app does around a typed command:

- before it: `COMMAND INFO` for the command (once per command and
  connection). A command that changes connection state, blocks or streams
  is not sent at all. `KEYS`, `DEBUG`, the flush commands, `SHUTDOWN`,
  `SWAPDB`, and administrative commands the server lists as dangerous are
  sent only after a confirmation; on a prod-tagged connection so is every
  command not known to be read-only;
- `COMMAND GETKEYS` when the connection is a cluster or the command may
  write;
- after a write: `TYPE`, `TTL` and `MEMORY USAGE` for the keys it named, so
  the index follows.

When `COMMAND` is denied the console still works: commands are judged by
name, everything unknown counts as a write, and a keyed command on a
cluster is sent to the chosen node and follows the server's redirect.

## Writes (CONCEPT section 7, write-through; section 8, safety)

Every mutation is one of these, applied to one key, followed by `EXISTS`
and `TTL` in the same pipeline so the index is updated in the same action.
Prod-tagged connections confirm every write in the UI.

| Command | Purpose |
|---|---|
| `UNLINK` | delete a key; pattern deletes are batched `UNLINK` calls (500 keys per call) |
| `DEL` | the same when `UNLINK` is unavailable; also the test suite |
| `RENAME` | rename a key |
| `EXPIRE`, `PERSIST` | set or remove an expiry |
| `SET` | edit a string (`KEEPTTL` keeps its expiry) or create one; the generator writes test data with it |
| `HSET`, `HDEL` | hash fields |
| `SADD`, `SREM` | set members |
| `ZADD`, `ZREM` | sorted set members |
| `LPUSH`, `RPUSH`, `LSET`, `LREM` | list items; removing by index is `LSET` to a sentinel then `LREM` |
| `XADD`, `XDEL` | stream entries |
| `FLUSHDB`, `FLUSHALL` | only after the user typed the connection's name |

## Test suite only

| Command | Purpose |
|---|---|
| `SELECT` | test suite |
| `ACL SETUSER`, `ACL DELUSER` | a restricted user for the tests of denied commands |
| `CLUSTER KEYSLOT` | checks the client-side slot computation against the server |

Example ACL for a browse-only user:

```
ACL SETUSER browser on >secret ~* +ping +info +dbsize +command|info +scan +type +ttl +memory|usage +exists \
  +object|encoding +strlen +getrange +hlen +hscan +scard +sscan +zcard +zscan +zrange +llen +lrange +xlen +xrange +dump \
  +cluster|shards +cluster|slots +readonly +asking +slowlog|len
```

To switch databases from the header add `+select` (the handshake sends it for
every database but 0) and `+config|get` for the number of databases.

For live updates add `+config|get +psubscribe +punsubscribe &__keyevent@*__:*`
(and `+config|set` only if the app may turn notifications on).

The same user with every write the browser can perform:

```
ACL SETUSER editor on >secret ~* +@read-browser +unlink +del +rename +expire +persist +set +hset +hdel +sadd +srem \
  +zadd +zrem +lpush +rpush +lset +lrem +xadd +xdel
```

(`@read-browser` stands for the browse list above; `FLUSHDB` and `FLUSHALL`
are deliberately left out of the example.)
