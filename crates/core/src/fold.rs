//! Folding of high-cardinality levels (CONCEPT section 5.6): the decision
//! rule over the per-class counters that ingestion maintains, plus labels.
//! Queries that feed it (summaries, buckets, similarity) live in the index.

use crate::index::ClassCounts;
use crate::segment::SegClass;

/// Global settings; the defaults are the concept's numbers.
#[derive(Debug, Clone, Copy)]
pub struct FoldThresholds {
    /// A folded level unfolds again only below this many children.
    pub unfold_below: i64,
    /// A level folds at this many children (and a class needs this many members).
    pub fold_at: i64,
    /// Above this, structural similarity is consulted when no class dominates.
    pub always_consider: i64,
    /// Share of distinct children a class (or all generated classes) must cover.
    pub class_share: f64,
    /// Average Jaccard overlap for the `{*}` fold.
    pub similarity: f64,
}

impl Default for FoldThresholds {
    fn default() -> Self {
        FoldThresholds {
            unfold_below: 50,
            fold_at: 100,
            always_consider: 2000,
            class_share: 0.9,
            similarity: 0.6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldClass {
    Uuid,
    Ulid,
    Hex,
    Numeric,
    Date,
    Opaque,
    Email,
    /// Structural fold of children without a recognisable shape.
    Star,
}

impl FoldClass {
    pub const GENERATED: [FoldClass; 7] = [
        FoldClass::Uuid,
        FoldClass::Ulid,
        FoldClass::Hex,
        FoldClass::Numeric,
        FoldClass::Date,
        FoldClass::Opaque,
        FoldClass::Email,
    ];

    pub fn seg_class(self) -> Option<SegClass> {
        Some(match self {
            FoldClass::Uuid => SegClass::Uuid,
            FoldClass::Ulid => SegClass::Ulid,
            FoldClass::Hex => SegClass::Hex,
            FoldClass::Numeric => SegClass::Numeric,
            FoldClass::Date => SegClass::Date,
            FoldClass::Opaque => SegClass::Opaque,
            FoldClass::Email => SegClass::Email,
            FoldClass::Star => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            FoldClass::Uuid => "uuid",
            FoldClass::Ulid => "ulid",
            FoldClass::Hex => "hex",
            FoldClass::Numeric => "numeric",
            FoldClass::Date => "date",
            FoldClass::Opaque => "opaque",
            FoldClass::Email => "email",
            FoldClass::Star => "star",
        }
    }

    pub fn parse(s: &str) -> Option<FoldClass> {
        match s {
            "uuid" => Some(FoldClass::Uuid),
            "ulid" => Some(FoldClass::Ulid),
            "hex" => Some(FoldClass::Hex),
            "numeric" => Some(FoldClass::Numeric),
            "date" => Some(FoldClass::Date),
            "opaque" => Some(FoldClass::Opaque),
            "email" => Some(FoldClass::Email),
            "star" => Some(FoldClass::Star),
            _ => None,
        }
    }

    /// Fold label. `hex_len` is the uniform segment length of a hex level;
    /// `numeric_range` the min and max of a numeric level, which turns the
    /// label into `{unix-ts}` for 10 or 13 digit values in a plausible range.
    pub fn label(self, hex_len: Option<i64>, numeric_range: Option<(i64, i64)>) -> String {
        match self {
            FoldClass::Uuid => "{uuid}".into(),
            FoldClass::Ulid => "{ulid}".into(),
            FoldClass::Hex => match hex_len {
                Some(n) => format!("{{hex{n}}}"),
                None => "{hex}".into(),
            },
            FoldClass::Numeric => match numeric_range {
                Some((lo, hi)) if is_unix_seconds(lo) && is_unix_seconds(hi) => "{unix-ts}".into(),
                Some((lo, hi)) if is_unix_millis(lo) && is_unix_millis(hi) => "{unix-ts}".into(),
                _ => "{n}".into(),
            },
            FoldClass::Date => "{date}".into(),
            FoldClass::Opaque => "{id}".into(),
            FoldClass::Email => "{email}".into(),
            FoldClass::Star => "{*}".into(),
        }
    }

    fn members(self, c: &ClassCounts) -> i64 {
        match self {
            FoldClass::Uuid => c.uuid,
            FoldClass::Ulid => c.ulid,
            FoldClass::Hex => c.hex,
            FoldClass::Numeric => c.numeric,
            FoldClass::Date => c.date,
            FoldClass::Opaque => c.opaque,
            FoldClass::Email => c.email,
            FoldClass::Star => 0,
        }
    }
}

/// 2001-09-09 to 2286-11-20 in seconds: the 10-digit range.
fn is_unix_seconds(v: i64) -> bool {
    (1_000_000_000..10_000_000_000).contains(&v)
}

/// The same range in milliseconds: 13 digits.
fn is_unix_millis(v: i64) -> bool {
    (1_000_000_000_000..10_000_000_000_000).contains(&v)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fold {
    pub class: FoldClass,
    pub members: i64,
    /// Share of the level's distinct children.
    pub share: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FoldDecision {
    pub folds: Vec<Fold>,
    /// Human-readable reasoning, for the "why folded?" tooltip.
    pub evidence: String,
}

impl FoldDecision {
    pub fn is_folded(&self) -> bool {
        !self.folds.is_empty()
    }
}

/// User rule for a level, from the connection profile later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Override {
    Force,
    Forbid,
}

/// Does the decision need structural similarity before it can be final?
/// Only when no class fold applies and the level is large.
pub fn needs_similarity(distinct: i64, classes: &ClassCounts, t: &FoldThresholds) -> bool {
    distinct >= t.always_consider && class_folds(distinct, classes, t).is_empty()
}

fn class_folds(distinct: i64, classes: &ClassCounts, t: &FoldThresholds) -> Vec<Fold> {
    if distinct <= 0 {
        return Vec::new();
    }
    let mut candidates: Vec<Fold> = FoldClass::GENERATED
        .iter()
        .map(|&c| Fold {
            class: c,
            members: c.members(classes),
            share: c.members(classes) as f64 / distinct as f64,
        })
        .filter(|f| f.members >= t.fold_at)
        .collect();
    candidates.sort_by_key(|f| std::cmp::Reverse(f.members));
    if let Some(top) = candidates.first()
        && top.share >= t.class_share
    {
        return vec![top.clone()];
    }
    let generated = classes.generated() as f64 / distinct as f64;
    if generated >= t.class_share && !candidates.is_empty() {
        return candidates;
    }
    Vec::new()
}

/// The decision rule. `similarity` is only consulted when
/// [`needs_similarity`] says so; passing `None` then means "not folded yet".
pub fn decide(distinct: i64, classes: &ClassCounts, similarity: Option<f64>, t: &FoldThresholds, rule: Option<Override>) -> FoldDecision {
    let pct = |n: i64| if distinct > 0 { 100.0 * n as f64 / distinct as f64 } else { 0.0 };
    let mut parts: Vec<String> = Vec::new();
    for c in FoldClass::GENERATED {
        let m = c.members(classes);
        if m > 0 {
            parts.push(format!("{:.1} % {}", pct(m), c.name()));
        }
    }
    if classes.literal > 0 {
        parts.push(format!("{:.1} % literal", pct(classes.literal)));
    }
    let mut evidence = format!("{} distinct", crate::fold::fmt_count(distinct));
    if !parts.is_empty() {
        evidence = format!("{evidence}: {}", parts.join(", "));
    }
    if let Some(s) = similarity {
        evidence = format!("{evidence}; similarity {s:.2}");
    }

    match rule {
        Some(Override::Forbid) => {
            return FoldDecision {
                folds: Vec::new(),
                evidence: format!("{evidence}; fold forbidden by rule"),
            };
        }
        Some(Override::Force) => {
            let mut folds = class_folds(distinct, classes, t);
            if folds.is_empty() {
                folds.push(Fold {
                    class: FoldClass::Star,
                    members: distinct,
                    share: 1.0,
                });
            }
            return FoldDecision {
                folds,
                evidence: format!("{evidence}; fold forced by rule"),
            };
        }
        None => {}
    }
    if distinct < t.fold_at {
        return FoldDecision {
            folds: Vec::new(),
            evidence: format!("{evidence}; below {}", t.fold_at),
        };
    }
    let folds = class_folds(distinct, classes, t);
    if !folds.is_empty() {
        return FoldDecision { folds, evidence };
    }
    if distinct >= t.always_consider
        && let Some(s) = similarity
        && s >= t.similarity
    {
        return FoldDecision {
            folds: vec![Fold {
                class: FoldClass::Star,
                members: distinct,
                share: 1.0,
            }],
            evidence,
        };
    }
    FoldDecision {
        folds: Vec::new(),
        evidence: format!("{evidence}; no dominant shape"),
    }
}

/// Thousands-separated count for labels and evidence.
pub fn fmt_count(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(numeric: i64, hex: i64, literal: i64) -> ClassCounts {
        ClassCounts {
            numeric,
            hex,
            literal,
            ..Default::default()
        }
    }

    #[test]
    fn dominant_class_folds_and_literals_stay() {
        let t = FoldThresholds::default();
        let d = decide(1002, &counts(1000, 0, 2), None, &t, None);
        assert_eq!(d.folds.len(), 1);
        assert_eq!(d.folds[0].class, FoldClass::Numeric);
        assert_eq!(d.folds[0].members, 1000);
        assert!(d.evidence.contains("99.8 % numeric"), "{}", d.evidence);
    }

    #[test]
    fn mixed_generated_classes_fold_side_by_side() {
        let t = FoldThresholds::default();
        let d = decide(1000, &counts(500, 450, 50), None, &t, None);
        let classes: Vec<FoldClass> = d.folds.iter().map(|f| f.class).collect();
        assert_eq!(classes, vec![FoldClass::Numeric, FoldClass::Hex]);
        // a class under fold_at members is not its own fold
        let d = decide(1000, &counts(910, 60, 30), None, &t, None);
        assert_eq!(d.folds.len(), 1);
        assert_eq!(d.folds[0].class, FoldClass::Numeric);
    }

    #[test]
    fn small_levels_and_literal_levels_do_not_fold() {
        let t = FoldThresholds::default();
        assert!(!decide(99, &counts(99, 0, 0), None, &t, None).is_folded());
        assert!(!decide(1000, &counts(100, 0, 900), None, &t, None).is_folded());
        assert!(!needs_similarity(1000, &counts(100, 0, 900), &t));
    }

    #[test]
    fn similarity_folds_large_literal_levels() {
        let t = FoldThresholds::default();
        let c = counts(0, 0, 5000);
        assert!(needs_similarity(5000, &c, &t));
        assert!(!decide(5000, &c, None, &t, None).is_folded());
        assert!(!decide(5000, &c, Some(0.3), &t, None).is_folded());
        let d = decide(5000, &c, Some(0.8), &t, None);
        assert_eq!(d.folds[0].class, FoldClass::Star);
    }

    #[test]
    fn overrides_win() {
        let t = FoldThresholds::default();
        assert!(!decide(1000, &counts(1000, 0, 0), None, &t, Some(Override::Forbid)).is_folded());
        let d = decide(10, &counts(0, 0, 10), None, &t, Some(Override::Force));
        assert_eq!(d.folds[0].class, FoldClass::Star);
        let d = decide(200, &counts(200, 0, 0), None, &t, Some(Override::Force));
        assert_eq!(d.folds[0].class, FoldClass::Numeric);
    }

    #[test]
    fn labels() {
        assert_eq!(FoldClass::Hex.label(Some(32), None), "{hex32}");
        assert_eq!(FoldClass::Hex.label(None, None), "{hex}");
        assert_eq!(FoldClass::Numeric.label(None, Some((1, 2_104_331))), "{n}");
        assert_eq!(FoldClass::Numeric.label(None, Some((1_700_000_000, 1_700_500_000))), "{unix-ts}");
        assert_eq!(
            FoldClass::Numeric.label(None, Some((1_700_000_000_000, 1_700_500_000_000))),
            "{unix-ts}"
        );
        assert_eq!(FoldClass::Star.label(None, None), "{*}");
        assert_eq!(fmt_count(2104331), "2,104,331");
    }
}
