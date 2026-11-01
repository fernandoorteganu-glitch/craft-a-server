//! Pure retention policy: which backups may be deleted.

use std::collections::{BTreeMap, HashSet};

const DAY: i64 = 86_400;

/// Why a backup was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackupKind {
    /// Scheduled daily backup.
    Daily,
    /// User-requested; never auto-deleted.
    Manual,
    /// Taken before a risky change (mod install, update).
    PreChange,
    /// Taken before a restore; counts as `PreChange`.
    PreRestore,
}

/// Metadata the policy needs about one backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupMeta {
    /// Unique id.
    pub id: String,
    /// Creation time, seconds since the Unix epoch.
    pub created_unix: i64,
    /// Why it was taken.
    pub kind: BackupKind,
    /// Whether verification succeeded.
    pub verified: bool,
}

/// Retention knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Newest UTC days that keep one daily backup each.
    pub keep_daily: usize,
    /// ISO weeks (older than the dailies) that keep one backup each.
    pub keep_weekly: usize,
    /// Newest PreChange/PreRestore backups kept.
    pub keep_pre_change: usize,
    /// Unverified backups younger than this are kept (verification may still be pending).
    pub unverified_grace_secs: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_daily: 7,
            keep_weekly: 4,
            keep_pre_change: 3,
            unverified_grace_secs: DAY,
        }
    }
}

/// Ids of backups that may be deleted under `policy`.
///
/// Rules, in order of precedence (anything matching a rule is kept):
/// - Manual backups are always kept.
/// - The newest verified backup is always kept; if no backup is verified, nothing is deleted.
/// - Backups dated in the future (clock skew) and unverified ones younger than the grace
///   period are kept.
/// - Daily: per UTC day the newest backup (preferring a verified one) represents the day; the
///   newest `keep_daily` days are kept. Of the older days, per ISO week the newest
///   representative is kept for the newest `keep_weekly` weeks.
/// - The newest `keep_pre_change` PreChange/PreRestore backups are kept.
///
/// Applying the result and calling again returns an empty list. Output is sorted by id.
pub fn select_deletions(
    backups: &[BackupMeta],
    now_unix: i64,
    policy: &RetentionPolicy,
) -> Vec<String> {
    if !backups.iter().any(|b| b.verified) {
        return Vec::new();
    }
    let mut keep: HashSet<&str> = HashSet::new();

    // Newest first, id as a deterministic tie-break.
    let newest_first = |a: &&BackupMeta, b: &&BackupMeta| {
        b.created_unix
            .cmp(&a.created_unix)
            .then_with(|| b.id.cmp(&a.id))
    };
    // Verified beats unverified, then newest first.
    let best_first = |a: &&BackupMeta, b: &&BackupMeta| {
        b.verified.cmp(&a.verified).then_with(|| newest_first(a, b))
    };

    if let Some(b) = backups.iter().filter(|b| b.verified).min_by(newest_first) {
        keep.insert(&b.id);
    }
    for b in backups {
        let future = b.created_unix > now_unix;
        let pending =
            !b.verified && now_unix.saturating_sub(b.created_unix) < policy.unverified_grace_secs;
        if b.kind == BackupKind::Manual || future || pending {
            keep.insert(&b.id);
        }
    }

    // Pre-change: newest N.
    let mut pre: Vec<&BackupMeta> = backups
        .iter()
        .filter(|b| matches!(b.kind, BackupKind::PreChange | BackupKind::PreRestore))
        .collect();
    pre.sort_by(newest_first);
    keep.extend(
        pre.iter()
            .take(policy.keep_pre_change)
            .map(|b| b.id.as_str()),
    );

    // Daily: group by UTC day, best representative per day.
    let mut by_day: BTreeMap<i64, Vec<&BackupMeta>> = BTreeMap::new();
    for b in backups.iter().filter(|b| b.kind == BackupKind::Daily) {
        by_day
            .entry(b.created_unix.div_euclid(DAY))
            .or_default()
            .push(b);
    }
    let reps: Vec<(i64, &BackupMeta)> = by_day
        .into_iter()
        .rev() // newest day first
        .filter_map(|(day, mut v)| {
            v.sort_by(best_first);
            v.first().map(|b| (day, *b))
        })
        .collect();
    keep.extend(
        reps.iter()
            .take(policy.keep_daily)
            .map(|(_, b)| b.id.as_str()),
    );

    // Weekly: older days grouped by ISO week (keyed by the week's Thursday), newest rep wins.
    let mut weeks: BTreeMap<i64, &BackupMeta> = BTreeMap::new();
    for (day, b) in reps.iter().skip(policy.keep_daily) {
        // Day 0 (1970-01-01) was a Thursday; weekday Monday=0.
        let weekday = (day + 3).rem_euclid(7);
        let thursday = day - weekday + 3;
        weeks.entry(thursday).or_insert(b); // days iterate newest first
    }
    keep.extend(
        weeks
            .values()
            .rev()
            .take(policy.keep_weekly)
            .map(|b| b.id.as_str()),
    );

    let mut out: Vec<String> = backups
        .iter()
        .filter(|b| !keep.contains(b.id.as_str()))
        .map(|b| b.id.clone())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NOW: i64 = 100 * DAY + 3600;

    fn m(id: &str, day: i64, hour: i64, kind: BackupKind, verified: bool) -> BackupMeta {
        BackupMeta {
            id: id.to_owned(),
            created_unix: day * DAY + hour * 3600,
            kind,
            verified,
        }
    }

    fn daily(id: &str, day: i64) -> BackupMeta {
        m(id, day, 3, BackupKind::Daily, true)
    }

    fn del(b: &[BackupMeta]) -> Vec<String> {
        select_deletions(b, NOW, &RetentionPolicy::default())
    }

    #[test]
    fn empty_and_small_sets_delete_nothing() {
        assert!(del(&[]).is_empty());
        let b: Vec<_> = (0..7).map(|i| daily(&format!("d{i}"), 90 + i)).collect();
        assert!(del(&b).is_empty());
    }

    #[test]
    fn keeps_seven_dailies_then_weekly() {
        // 60 consecutive days ending at day 99.
        let b: Vec<_> = (40..100).map(|d| daily(&format!("d{d}"), d)).collect();
        let deleted = del(&b);
        let kept: Vec<_> = b
            .iter()
            .filter(|x| !deleted.contains(&x.id))
            .map(|x| x.id.clone())
            .collect();
        // Last 7 days kept.
        for d in 93..100 {
            assert!(kept.contains(&format!("d{d}")), "d{d}");
        }
        // 4 weekly extras -> 11 total.
        assert_eq!(kept.len(), 11, "{kept:?}");
        // Day 0 was a Thursday, so Mon 88..Sun 94 is one ISO week; dailies cover 93..99 and
        // the newest older day of that week (92) is its weekly. Next weeks: 87, 80, 73.
        assert!(kept.contains(&"d92".to_owned()));
        assert!(kept.contains(&"d87".to_owned()));
        assert!(kept.contains(&"d80".to_owned()));
        assert!(kept.contains(&"d73".to_owned()));
        assert!(!kept.contains(&"d66".to_owned()));
    }

    #[test]
    fn one_per_day_newest_wins() {
        let mut b = vec![
            m("early", 99, 1, BackupKind::Daily, true),
            m("late", 99, 20, BackupKind::Daily, true),
        ];
        assert_eq!(del(&b), vec!["early"]);
        // Verified preferred over a newer unverified (older than grace).
        b = vec![
            m("old-ok", 50, 1, BackupKind::Daily, true),
            m("old-bad", 50, 20, BackupKind::Daily, false),
        ];
        assert_eq!(del(&b), vec!["old-bad"]);
    }

    #[test]
    fn manual_is_never_deleted() {
        let mut b: Vec<_> = (0..30).map(|d| daily(&format!("d{d}"), d)).collect();
        for i in 0..5 {
            b.push(m(&format!("man{i}"), i, 5, BackupKind::Manual, i % 2 == 0));
        }
        let d = del(&b);
        assert!(!d.iter().any(|id| id.starts_with("man")));
        assert!(!d.is_empty());
    }

    #[test]
    fn pre_change_and_pre_restore_share_a_budget_of_three() {
        let b = vec![
            m("p1", 10, 1, BackupKind::PreChange, true),
            m("p2", 20, 1, BackupKind::PreRestore, true),
            m("p3", 30, 1, BackupKind::PreChange, true),
            m("p4", 40, 1, BackupKind::PreRestore, true),
            m("p5", 50, 1, BackupKind::PreChange, true),
        ];
        assert_eq!(del(&b), vec!["p1", "p2"]);
    }

    #[test]
    fn newest_verified_is_protected_even_if_old_or_odd_kind() {
        // Only verified backup is an ancient PreChange outside every budget.
        let mut b = vec![m("ancient", 1, 1, BackupKind::PreChange, true)];
        for i in 0..5 {
            b.push(m(&format!("n{i}"), 10 + i, 1, BackupKind::PreChange, false));
        }
        let d = del(&b);
        assert!(!d.contains(&"ancient".to_owned()));
    }

    #[test]
    fn no_verified_backup_means_no_deletions() {
        let b: Vec<_> = (0..30)
            .map(|d| m(&format!("d{d}"), d, 1, BackupKind::Daily, false))
            .collect();
        assert!(del(&b).is_empty());
    }

    #[test]
    fn unverified_grace_and_future_dated_are_kept() {
        let b = vec![
            daily("ok", 10),
            m("fresh-bad", 100, 0, BackupKind::PreChange, false),
            m("old-bad", 20, 0, BackupKind::PreChange, false),
            m("future", 200, 0, BackupKind::Daily, true),
        ];
        // Budgets are big enough for pre-change, so shrink them to force the other rules.
        let p = RetentionPolicy {
            keep_pre_change: 0,
            keep_daily: 0,
            keep_weekly: 0,
            ..RetentionPolicy::default()
        };
        // "future" is verified & newest -> protected; "ok" is not.
        let d = select_deletions(&b, NOW, &p);
        assert_eq!(d, vec!["ok", "old-bad"]);
    }

    #[test]
    fn iso_week_boundaries() {
        // keep_daily=0 so every day is "older". Mon 2024-12-30 and Sun 2025-01-05 share ISO
        // week 2025-W01; Mon 2024-12-23 is W52. Days since epoch: 2024-12-30 = 20087.
        let base = 20_087;
        let p = RetentionPolicy {
            keep_daily: 0,
            keep_weekly: 10,
            keep_pre_change: 0,
            unverified_grace_secs: 0,
        };
        let b = vec![
            daily("mon", base),
            daily("sun", base + 6),
            daily("prev-sun", base - 1),
            daily("prev-mon", base - 7),
        ];
        let now = (base + 30) * DAY;
        let d = select_deletions(&b, now, &p);
        assert_eq!(d, vec!["mon", "prev-mon"]);
    }

    #[test]
    fn negative_timestamps_do_not_panic() {
        let b = vec![
            m("a", -5, 0, BackupKind::Daily, true),
            m("b", -400, 0, BackupKind::Daily, true),
        ];
        let p = RetentionPolicy {
            keep_daily: 1,
            keep_weekly: 0,
            ..RetentionPolicy::default()
        };
        assert_eq!(select_deletions(&b, 0, &p), vec!["b"]);
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let b = vec![
            BackupMeta {
                id: "min".into(),
                created_unix: i64::MIN,
                kind: BackupKind::Daily,
                verified: true,
            },
            BackupMeta {
                id: "max".into(),
                created_unix: i64::MAX,
                kind: BackupKind::Daily,
                verified: false,
            },
        ];
        let _ = select_deletions(&b, i64::MIN, &RetentionPolicy::default());
        let _ = select_deletions(&b, i64::MAX, &RetentionPolicy::default());
    }

    fn arb_kind() -> impl Strategy<Value = BackupKind> {
        prop_oneof![
            Just(BackupKind::Daily),
            Just(BackupKind::Manual),
            Just(BackupKind::PreChange),
            Just(BackupKind::PreRestore),
        ]
    }

    fn arb_set() -> impl Strategy<Value = Vec<BackupMeta>> {
        prop::collection::vec((0i64..120 * DAY, arb_kind(), any::<bool>()), 0..60).prop_map(|v| {
            v.into_iter()
                .enumerate()
                .map(|(i, (t, kind, verified))| BackupMeta {
                    id: format!("b{i:03}"),
                    created_unix: t,
                    kind,
                    verified,
                })
                .collect()
        })
    }

    fn arb_policy() -> impl Strategy<Value = RetentionPolicy> {
        (0usize..10, 0usize..6, 0usize..5, 0i64..3 * DAY).prop_map(|(d, w, p, g)| RetentionPolicy {
            keep_daily: d,
            keep_weekly: w,
            keep_pre_change: p,
            unverified_grace_secs: g,
        })
    }

    proptest! {
        #[test]
        fn invariants(set in arb_set(), policy in arb_policy(), now in 0i64..130 * DAY) {
            let deleted = select_deletions(&set, now, &policy);
            let del: HashSet<&str> = deleted.iter().map(String::as_str).collect();

            // Only known ids, no duplicates.
            prop_assert_eq!(del.len(), deleted.len());
            for id in &del {
                prop_assert!(set.iter().any(|b| b.id == *id));
            }
            // Never deletes manual.
            for b in &set {
                if b.kind == BackupKind::Manual {
                    prop_assert!(!del.contains(b.id.as_str()));
                }
            }
            // Never deletes the newest verified.
            let newest = set.iter().filter(|b| b.verified)
                .max_by(|a, b| a.created_unix.cmp(&b.created_unix).then_with(|| a.id.cmp(&b.id)));
            if let Some(n) = newest {
                prop_assert!(!del.contains(n.id.as_str()));
            }
            // A verified backup survives whenever one existed.
            let had = set.iter().any(|b| b.verified);
            let left = set.iter().any(|b| b.verified && !del.contains(b.id.as_str()));
            prop_assert!(!had || left);

            // Idempotent.
            let rest: Vec<BackupMeta> = set.iter().filter(|b| !del.contains(b.id.as_str())).cloned().collect();
            prop_assert!(select_deletions(&rest, now, &policy).is_empty());

            // Order of input does not matter.
            let mut rev = set.clone();
            rev.reverse();
            prop_assert_eq!(select_deletions(&rev, now, &policy), deleted);
        }
    }
}
