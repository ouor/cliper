//! Interval sets on the VOD timeline: `(start, end)` seconds, half-open,
//! kept sorted and merged. Used for video coverage, fetch requests, separated
//! ranges, talk-scanned ranges and cleanup selection.

pub type Range = (f64, f64);

/// Sort, drop empty/invalid intervals and merge overlapping or touching ones
/// (gaps up to `join` seconds are closed).
pub fn normalize_with(v: &[Range], join: f64) -> Vec<Range> {
    let mut v: Vec<Range> = v.iter().copied().filter(|(a, b)| a.is_finite() && b.is_finite() && b > a).collect();
    v.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out: Vec<Range> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 + join => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// [`normalize_with`] without closing gaps (touching intervals still merge).
pub fn normalize(v: &[Range]) -> Vec<Range> {
    normalize_with(v, 0.0)
}

pub fn union(a: &[Range], b: &[Range]) -> Vec<Range> {
    let mut v = a.to_vec();
    v.extend_from_slice(b);
    normalize(&v)
}

/// `a ∩ b` (both may be unnormalized).
pub fn intersect(a: &[Range], b: &[Range]) -> Vec<Range> {
    let (a, b) = (normalize(a), normalize(b));
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        if hi > lo {
            out.push((lo, hi));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// `a − b`.
pub fn subtract(a: &[Range], b: &[Range]) -> Vec<Range> {
    let (a, b) = (normalize(a), normalize(b));
    let mut out = Vec::new();
    for &(mut lo, hi) in &a {
        for &(x, y) in &b {
            if y <= lo || x >= hi {
                continue;
            }
            if x > lo {
                out.push((lo, x));
            }
            lo = lo.max(y);
            if lo >= hi {
                break;
            }
        }
        if lo < hi {
            out.push((lo, hi));
        }
    }
    out
}

pub fn total(a: &[Range]) -> f64 {
    normalize(a).iter().map(|(x, y)| y - x).sum()
}

/// Is `r` fully inside the union of `a` (within `eps` at the edges)?
pub fn covers(a: &[Range], r: Range, eps: f64) -> bool {
    subtract(&[(r.0 + eps, r.1 - eps)], a).is_empty()
}

/// Drop pieces shorter than `min`.
pub fn drop_short(a: Vec<Range>, min: f64) -> Vec<Range> {
    a.into_iter().filter(|(x, y)| y - x >= min).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_ops() {
        let a = normalize(&[(5.0, 7.0), (0.0, 2.0), (1.0, 3.0), (3.0, 4.0), (9.0, 9.0)]);
        assert_eq!(a, vec![(0.0, 4.0), (5.0, 7.0)]);
        assert_eq!(normalize_with(&a, 1.0), vec![(0.0, 7.0)]);
        assert_eq!(subtract(&[(0.0, 10.0)], &[(2.0, 3.0), (5.0, 6.0)]), vec![(0.0, 2.0), (3.0, 5.0), (6.0, 10.0)]);
        assert_eq!(subtract(&[(0.0, 10.0)], &[(-1.0, 11.0)]), vec![]);
        assert_eq!(intersect(&[(0.0, 4.0), (5.0, 7.0)], &[(3.0, 6.0)]), vec![(3.0, 4.0), (5.0, 6.0)]);
        assert_eq!(union(&[(0.0, 1.0)], &[(1.0, 2.0), (4.0, 5.0)]), vec![(0.0, 2.0), (4.0, 5.0)]);
        assert!((total(&[(0.0, 1.0), (0.5, 2.0), (3.0, 3.5)]) - 2.5).abs() < 1e-12);
        assert!(covers(&[(0.0, 4.0), (4.0, 8.0)], (1.0, 7.0), 0.0));
        assert!(!covers(&[(0.0, 4.0), (4.5, 8.0)], (1.0, 7.0), 0.0));
        assert!(covers(&[(0.0, 4.0)], (0.0, 4.05), 0.1));
        assert_eq!(drop_short(vec![(0.0, 0.1), (1.0, 3.0)], 0.5), vec![(1.0, 3.0)]);
    }
}
