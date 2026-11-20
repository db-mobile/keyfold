//! Test-data generator: a realistic keyspace with mixed namespaces and
//! id shapes, written with pipelined `SET`; on a cluster one pipeline per
//! owning node.

use std::time::Instant;

use anyhow::Result;
use keyfold_core::commands::{self, testing};
use keyfold_core::route::{Hop, Router};
use keyfold_core::routed;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex(rng: &mut SmallRng, len: usize) -> String {
    (0..len).map(|_| HEX[rng.random_range(0..16)] as char).collect()
}

fn uuid(rng: &mut SmallRng) -> String {
    format!("{}-{}-4{}-{}-{}", hex(rng, 8), hex(rng, 4), hex(rng, 3), hex(rng, 4), hex(rng, 12))
}

/// Deterministic key number `i` of a keyspace with `total` keys. Every `i`
/// yields a distinct key, so `DBSIZE` equals `total` after a flush.
///
/// Shapes, by weight:
/// - `user:{n}:profile|cart|settings:{literal}|sessions:{uuid}` (55 %)
/// - `order:{hex32}` and `order:{hex32}:items` (20 %)
/// - `cache:{date}:{n}` (10 %)
/// - `tenant:{n}:user:{n}:session:{uuid}` (10 %)
/// - `event:{unix-ts}` with a few literal keys (5 %)
pub fn key_name(i: u64, _total: u64, rng: &mut SmallRng) -> String {
    let block = i / 20;
    match i % 20 {
        s @ 0..=6 => format!("user:{}:profile", block * 7 + s),
        7..=8 => format!("user:{}:sessions:{}", block, uuid(rng)),
        9 => format!("user:{}:cart", block),
        10 => format!(
            "user:{}:settings:{}",
            block,
            ["theme", "locale", "notifications"][(block % 3) as usize]
        ),
        11..=13 => format!("order:{}", hex(rng, 32)),
        14 => format!("order:{}:items", hex(rng, 32)),
        15..=16 => format!("cache:2024-{:02}-{:02}:{}", 1 + block % 12, 1 + block % 28, i),
        17..=18 => format!("tenant:{}:user:{}:session:{}", block % 50, block, uuid(rng)),
        _ => {
            if block.is_multiple_of(50) {
                format!("meta:{}:{}", ["version", "schema", "lock", "leader"][(block % 4) as usize], block)
            } else {
                format!("event:{}", 1_700_000_000 + i)
            }
        }
    }
}

/// Write `total` keys, `batch` per pipeline. Returns the number of SET calls
/// and the elapsed time.
pub async fn generate(
    router: &mut Router,
    total: u64,
    batch: usize,
    flush: bool,
    progress: impl Fn(u64),
) -> Result<(u64, std::time::Duration)> {
    if flush {
        router.flush(false).await?;
    }
    let mut rng = SmallRng::seed_from_u64(42);
    let started = Instant::now();
    let mut sent = 0u64;
    let mut i = 0u64;
    while i < total {
        let end = (i + batch as u64).min(total);
        let names: Vec<String> = (i..end).map(|n| key_name(n, total, &mut rng)).collect();
        let refs: Vec<&[u8]> = names.iter().map(|k| k.as_bytes()).collect();
        for (addr, idxs) in router.group_by_node(&refs) {
            let mut pipe = commands::pipeline();
            for &k in &idxs {
                pipe.add_command(testing::set(refs[k], b"1"));
            }
            for r in routed!(router, Hop::node(&addr), |c| c.pipeline(&pipe).await)? {
                r?;
            }
        }
        sent += end - i;
        i = end;
        progress(sent);
    }
    Ok((sent, started.elapsed()))
}

/// Name number `i` of the keys that are not valid UTF-8. They sit under
/// `bin:` in a few shapes: lone bytes above 0x7f, a NUL, a truncated
/// multi-byte sequence, a child under a parent that is itself a key, and
/// a literal backslash next to an invalid byte.
pub fn binary_key_name(i: u64) -> Vec<u8> {
    let n = (i / 5).to_string();
    let n = n.as_bytes();
    let lo = (i / 5 % 64) as u8;
    match i % 5 {
        0 => [b"bin:", &[0xff, 0x80 | lo][..], b":", n].concat(),
        1 => [b"bin:nul:", n, &[0x00, 0xfe][..]].concat(),
        2 => [b"bin:cut:", n, &[0xc3][..]].concat(),
        3 => [b"bin:", &[0xff, 0x80 | lo][..], b":", n, b":child"].concat(),
        _ => [b"bin:esc:", n, b"\\x41", &[0xf0 | (lo & 7)][..]].concat(),
    }
}

/// Write `total` keys with names that are not valid UTF-8, on top of
/// whatever is there. Returns the number of SET calls.
pub async fn generate_binary(router: &mut Router, total: u64, batch: usize) -> Result<u64> {
    let mut i = 0u64;
    while i < total {
        let end = (i + batch as u64).min(total);
        let names: Vec<Vec<u8>> = (i..end).map(binary_key_name).collect();
        let refs: Vec<&[u8]> = names.iter().map(|k| &k[..]).collect();
        for (addr, idxs) in router.group_by_node(&refs) {
            let mut pipe = commands::pipeline();
            for &k in &idxs {
                pipe.add_command(testing::set(refs[k], b"1"));
            }
            for r in routed!(router, Hop::node(&addr), |c| c.pipeline(&pipe).await)? {
                r?;
            }
        }
        i = end;
    }
    Ok(total)
}

pub async fn dbsize(router: &mut Router) -> Result<u64> {
    Ok(router.dbsize().await.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_names_are_distinct_and_not_utf8() {
        let names: std::collections::HashSet<Vec<u8>> = (0..5000).map(binary_key_name).collect();
        assert_eq!(names.len(), 5000);
        assert!(names.iter().all(|n| std::str::from_utf8(n).is_err() && n.starts_with(b"bin:")));
    }
}
