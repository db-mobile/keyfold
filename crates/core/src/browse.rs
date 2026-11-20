//! Browsing without an index (CONCEPT 7, "no index"): plain `SCAN` pages,
//! one after another, with nothing kept but the cursor. The floor for
//! locked-down or read-only connections; the tree, counts and search need
//! the index and are not offered here.
//!
//! A [`Pager`] walks the nodes it was given in turn (the nodes a scan
//! would read, so replicas are honoured) and hands out pages of at most
//! `want` keys. `SCAN` gives no ordering and no total; with a `MATCH`
//! pattern the server still walks its whole table, so a page is also
//! bounded by calls and time and may come back short, or empty, with more
//! to come.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::commands;
use crate::error::Result;
use crate::scan::{ScanNode, parse_scan_reply};

/// Limits of one page.
#[derive(Debug, Clone, Copy)]
pub struct PageOptions {
    /// Keys in a page at most (the windowing invariant: 200).
    pub want: usize,
    /// `COUNT` while a pattern or type filters the replies; without a
    /// filter `COUNT` is what the page still needs.
    pub filter_count: usize,
    /// `SCAN` calls in a page at most.
    pub max_calls: u32,
    /// Time for a page at most; checked between calls.
    pub max_time: Duration,
    /// Pause between two calls (the scan profile's gap).
    pub gap: Duration,
}

impl Default for PageOptions {
    fn default() -> Self {
        PageOptions {
            want: 200,
            filter_count: 5000,
            max_calls: 40,
            max_time: Duration::from_secs(1),
            gap: Duration::ZERO,
        }
    }
}

/// One page of key names, in the server's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub keys: Vec<Vec<u8>>,
    /// Every node has been walked to the end and nothing is buffered.
    pub done: bool,
    /// `SCAN` calls this page took.
    pub calls: u32,
}

/// A walk over the keyspace in pages. Dropping it ends the walk; there is
/// nothing to clean up on the server.
pub struct Pager {
    nodes: Vec<ScanNode>,
    node: usize,
    cursor: u64,
    finished: bool,
    /// Keys a reply returned beyond what its page could take.
    buffer: VecDeque<Vec<u8>>,
    pattern: Option<Vec<u8>>,
    type_filter: Option<String>,
    calls: u64,
    examined: u64,
    returned: u64,
}

impl Pager {
    /// `pattern` is a glob for `SCAN MATCH`, `type_filter` a `TYPE` name.
    pub fn new(nodes: Vec<ScanNode>, pattern: Option<Vec<u8>>, type_filter: Option<String>) -> Pager {
        Pager {
            finished: nodes.is_empty(),
            nodes,
            node: 0,
            cursor: 0,
            buffer: VecDeque::new(),
            pattern,
            type_filter,
            calls: 0,
            examined: 0,
            returned: 0,
        }
    }

    /// The next page. After an error the walk can go on: the failed call
    /// is repeated and no key that was already read is lost.
    pub async fn next_page(&mut self, opts: &PageOptions) -> Result<Page> {
        let want = opts.want.max(1);
        let filtered = self.pattern.is_some() || self.type_filter.is_some();
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(want);
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        while keys.len() < want
            && let Some(k) = self.buffer.pop_front()
        {
            if seen.insert(k.clone()) {
                keys.push(k);
            }
        }
        let started = Instant::now();
        let mut calls = 0u32;
        while keys.len() < want && !self.finished {
            if calls >= opts.max_calls.max(1) || (calls > 0 && started.elapsed() >= opts.max_time) {
                break;
            }
            if calls > 0 && !opts.gap.is_zero() {
                tokio::time::sleep(opts.gap).await;
            }
            let count = if filtered {
                opts.filter_count.max(1)
            } else {
                (want - keys.len()).max(10)
            };
            let cmd = commands::scan(self.cursor, self.pattern.as_deref(), count, self.type_filter.as_deref());
            let reply = match self.nodes[self.node].conn.raw(&cmd).await {
                Ok(v) => parse_scan_reply(v),
                Err(e) => Err(e),
            };
            let (next, batch) = match reply {
                Ok(r) => r,
                Err(e) => {
                    // what this page had gathered is served by the next one
                    for k in keys.into_iter().rev() {
                        self.buffer.push_front(k);
                    }
                    return Err(e);
                }
            };
            calls += 1;
            self.calls += 1;
            self.examined += count as u64;
            for k in batch {
                // SCAN may return a key more than once
                if !seen.insert(k.clone()) {
                    continue;
                }
                if keys.len() < want {
                    keys.push(k);
                } else {
                    self.buffer.push_back(k);
                }
            }
            self.cursor = next;
            if next == 0 {
                self.node += 1;
                self.finished = self.node >= self.nodes.len();
            }
        }
        self.returned += keys.len() as u64;
        Ok(Page {
            keys,
            done: self.is_done(),
            calls,
        })
    }

    pub fn is_done(&self) -> bool {
        self.finished && self.buffer.is_empty()
    }

    /// `SCAN` calls so far.
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// The sum of `COUNT` over the calls so far: roughly how many keys the
    /// server has walked, for "about N of DBSIZE examined".
    pub fn examined(&self) -> u64 {
        self.examined
    }

    /// Keys handed out so far.
    pub fn returned(&self) -> u64 {
        self.returned
    }

    /// The node being read, and how many there are.
    pub fn position(&self) -> (Option<&str>, usize, usize) {
        (
            self.nodes.get(self.node).map(|n| n.label.as_str()),
            self.node.min(self.nodes.len()),
            self.nodes.len(),
        )
    }
}
