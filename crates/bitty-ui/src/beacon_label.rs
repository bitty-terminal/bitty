//! Beacon label allocation (UX-30, U-8 Beacon family).
//!
//! [`LabelAllocator`] assigns one unique hint label per beacon target.
//! Single-character home-row labels come first; overflow spills to two
//! characters. Targets left of the viewport center draw their leading
//! character from the left-hand pool and targets right of center from the
//! right-hand pool (spatial pools), so common two-target sessions stay on
//! opposite hands.
//!
//! The character sets are Lua policy, not Rust constants: the caller passes
//! a [`LabelPolicy`] (owned by the Lua hint configuration) and this module
//! only validates and consumes it. [`LabelPolicy::default_policy`] is the
//! built-in home-row fallback used when Lua supplies no charset.
//!
//! Allocation is deterministic (order by `(side, row, column)`, pools in
//! policy order) and bounded ([`MAX_BEACON_TARGETS`]); exhaustion fails
//! closed with [`LabelError::TooManyTargets`]. Headless and pure: no I/O,
//! wall-clock, or randomness.

#![forbid(unsafe_code)]

use crate::geometry::Point;

/// Absolute cap on labels per hint session.
pub const MAX_BEACON_TARGETS: usize = 1024;

/// Maximum characters accepted in one policy charset.
pub const MAX_CHARSET_LEN: usize = 64;

/// Built-in home-row fallback (`asdfghjkl`), used when Lua supplies no
/// charset. Left-hand pool is the first half, right-hand pool the second.
pub const DEFAULT_HOME_CHARSET: &str = "asdfghjkl";

/// Label allocation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelError {
    /// A charset is empty, too long, non-`[a-z0-9]`, or has duplicates.
    InvalidCharset(String),
    /// The session needs more distinct labels than the policy can generate.
    TooManyTargets {
        /// Requested target count.
        requested: usize,
        /// Distinct labels the policy can generate.
        capacity: usize,
    },
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCharset(detail) => write!(f, "invalid label charset: {detail}"),
            Self::TooManyTargets {
                requested,
                capacity,
            } => write!(
                f,
                "too many beacon targets: requested {requested}, capacity {capacity}"
            ),
        }
    }
}

impl std::error::Error for LabelError {}

/// Charset policy for label allocation. Supplied by the Lua hint
/// configuration; Rust never invents characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelPolicy {
    home: Vec<char>,
    overflow: Vec<char>,
}

impl LabelPolicy {
    /// Builds a policy from Lua-supplied charsets. Both sets must be
    /// non-empty, `<= [`MAX_CHARSET_LEN`]` chars, all ASCII `[a-z0-9]`, and
    /// duplicate-free within the set.
    pub fn new(home: &str, overflow: &str) -> Result<Self, LabelError> {
        Ok(Self {
            home: validate_charset(home, "home")?,
            overflow: validate_charset(overflow, "overflow")?,
        })
    }

    /// Built-in home-row fallback: home and overflow both
    /// [`DEFAULT_HOME_CHARSET`].
    #[must_use]
    pub fn default_policy() -> Self {
        Self {
            home: DEFAULT_HOME_CHARSET.chars().collect(),
            overflow: DEFAULT_HOME_CHARSET.chars().collect(),
        }
    }

    /// Home-row characters in policy order.
    #[must_use]
    pub fn home(&self) -> &[char] {
        &self.home
    }

    /// Overflow second-character set in policy order.
    #[must_use]
    pub fn overflow(&self) -> &[char] {
        &self.overflow
    }

    /// Left-hand pool: first half of `home` (rounded down, at least one).
    #[must_use]
    pub fn left_pool(&self) -> &[char] {
        let mid = (self.home.len() / 2).max(1).min(self.home.len());
        &self.home[..mid]
    }

    /// Right-hand pool: second half of `home` (falls back to full `home`
    /// when `home` has a single character).
    #[must_use]
    pub fn right_pool(&self) -> &[char] {
        let mid = (self.home.len() / 2).max(1).min(self.home.len());
        if mid >= self.home.len() {
            &self.home
        } else {
            &self.home[mid..]
        }
    }
}

impl Default for LabelPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// Validates one Lua-supplied charset.
fn validate_charset(raw: &str, role: &str) -> Result<Vec<char>, LabelError> {
    let chars: Vec<char> = raw.chars().collect();
    if chars.is_empty() {
        return Err(LabelError::InvalidCharset(format!(
            "{role} charset is empty"
        )));
    }
    if chars.len() > MAX_CHARSET_LEN {
        return Err(LabelError::InvalidCharset(format!(
            "{role} charset exceeds {MAX_CHARSET_LEN} chars"
        )));
    }
    if !chars
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(LabelError::InvalidCharset(format!(
            "{role} charset must be [a-z0-9]"
        )));
    }
    let mut seen = [false; 128];
    for c in &chars {
        let idx = *c as usize;
        if seen[idx] {
            return Err(LabelError::InvalidCharset(format!(
                "{role} charset has duplicate '{c}'"
            )));
        }
        seen[idx] = true;
    }
    Ok(chars)
}

/// Deterministic hint-label allocator over a [`LabelPolicy`].
#[derive(Clone, Debug)]
pub struct LabelAllocator {
    policy: LabelPolicy,
}

impl LabelAllocator {
    /// Creates an allocator over `policy`.
    #[must_use]
    pub fn new(policy: LabelPolicy) -> Self {
        Self { policy }
    }

    /// Returns the active policy.
    #[must_use]
    pub fn policy(&self) -> &LabelPolicy {
        &self.policy
    }

    /// Assigns one unique label per anchor, returned in input order.
    ///
    /// Anchors with `x * 2 < viewport_width` count as left-of-center and
    /// take the left pool; the rest take the right pool. Within a side,
    /// targets are served in `(y, x)` order. Each side serves its pool
    /// singles first, then two-character overflow (`pool[i] + overflow[j]`,
    /// `i` outermost). Fails closed when the request exceeds
    /// [`MAX_BEACON_TARGETS`] or the policy's generable space.
    pub fn assign(
        &self,
        anchors: &[Point],
        viewport_width: u16,
    ) -> Result<Vec<String>, LabelError> {
        if anchors.len() > MAX_BEACON_TARGETS {
            return Err(LabelError::TooManyTargets {
                requested: anchors.len(),
                capacity: self.capacity(),
            });
        }
        // Partition input indices into left/right sides, each in (y, x)
        // order for determinism.
        let mut left: Vec<usize> = Vec::new();
        let mut right: Vec<usize> = Vec::new();
        for (index, anchor) in anchors.iter().enumerate() {
            if (u32::from(anchor.x) * 2) < u32::from(viewport_width) {
                left.push(index);
            } else {
                right.push(index);
            }
        }
        left.sort_by_key(|&i| (anchors[i].y, anchors[i].x));
        right.sort_by_key(|&i| (anchors[i].y, anchors[i].x));

        let left_labels = side_labels(self.policy.left_pool(), &self.policy, left.len())?;
        let right_labels = side_labels(self.policy.right_pool(), &self.policy, right.len())?;

        let mut out: Vec<String> = vec![String::new(); anchors.len()];
        for (position, index) in left.iter().enumerate() {
            out[*index] = left_labels[position].clone();
        }
        for (position, index) in right.iter().enumerate() {
            out[*index] = right_labels[position].clone();
        }
        Ok(out)
    }

    /// Distinct labels this policy can generate across both pools.
    #[must_use]
    pub fn capacity(&self) -> usize {
        side_capacity(self.policy.left_pool().len(), self.policy.overflow.len())
            + side_capacity(self.policy.right_pool().len(), self.policy.overflow.len())
    }
}

impl Default for LabelAllocator {
    fn default() -> Self {
        Self::new(LabelPolicy::default_policy())
    }
}

/// Distinct labels one side pool can serve: singles plus `pool x overflow`
/// pairs.
fn side_capacity(pool_len: usize, overflow_len: usize) -> usize {
    pool_len + pool_len.saturating_mul(overflow_len)
}

/// Serves `count` labels from `pool`: singles first, then two-character
/// overflow. Errors when the side's space is exhausted.
fn side_labels(
    pool: &[char],
    policy: &LabelPolicy,
    count: usize,
) -> Result<Vec<String>, LabelError> {
    let capacity = side_capacity(pool.len(), policy.overflow.len());
    if count > capacity {
        return Err(LabelError::TooManyTargets {
            requested: count,
            capacity,
        });
    }
    let mut labels = Vec::with_capacity(count);
    for c in pool {
        if labels.len() >= count {
            break;
        }
        labels.push(c.to_string());
    }
    'outer: for first in pool {
        for second in &policy.overflow {
            if labels.len() >= count {
                break 'outer;
            }
            labels.push(format!("{first}{second}"));
        }
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(cols: &[u16]) -> Vec<Point> {
        cols.iter()
            .enumerate()
            .map(|(i, &x)| Point::new(x, i as u16))
            .collect()
    }

    #[test]
    fn home_row_first_in_policy_order() {
        let allocator = LabelAllocator::default();
        // All anchors right-of-center still draw the home row first.
        let anchors = points(&[60, 61, 62]);
        let labels = allocator.assign(&anchors, 80).expect("labels");
        assert_eq!(labels, vec!["g", "h", "j"]);
    }

    #[test]
    fn spatial_pools_split_by_center() {
        let allocator = LabelAllocator::default();
        // x=10 is left-of-center (pool asdf), x=70 right (pool ghjkl).
        let anchors = points(&[10, 70]);
        let labels = allocator.assign(&anchors, 80).expect("labels");
        assert_eq!(labels, vec!["a", "g"]);
    }

    #[test]
    fn two_char_overflow_after_singles() {
        let policy = LabelPolicy::new("ab", "ab").expect("policy");
        let allocator = LabelAllocator::new(policy);
        // Left pool "a" (x<40), right pool "b": force 3 left targets so the
        // third spills to two-char overflow "aa".
        let anchors = vec![
            Point::new(1, 0),
            Point::new(2, 1),
            Point::new(3, 2),
            Point::new(70, 0),
        ];
        let labels = allocator.assign(&anchors, 80).expect("labels");
        assert_eq!(labels, vec!["a", "aa", "ab", "b"]);
    }

    #[test]
    fn labels_unique_and_deterministic() {
        let allocator = LabelAllocator::default();
        let anchors = points(&[5, 70, 6, 71, 7, 72, 8, 73]);
        let first = allocator.assign(&anchors, 80).expect("labels");
        let second = allocator.assign(&anchors, 80).expect("labels");
        assert_eq!(first, second);
        let mut sorted = first.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), first.len());
    }

    #[test]
    fn lua_charset_used_verbatim() {
        let policy = LabelPolicy::new("zxcv", "zxcv").expect("policy");
        let allocator = LabelAllocator::new(policy);
        let anchors = points(&[70, 71]);
        let labels = allocator.assign(&anchors, 80).expect("labels");
        // Right pool is the second half "cv".
        assert_eq!(labels, vec!["c", "v"]);
    }

    #[test]
    fn invalid_charsets_rejected() {
        assert!(matches!(
            LabelPolicy::new("", "ab"),
            Err(LabelError::InvalidCharset(_))
        ));
        assert!(matches!(
            LabelPolicy::new("aA", "ab"),
            Err(LabelError::InvalidCharset(_))
        ));
        assert!(matches!(
            LabelPolicy::new("aa", "ab"),
            Err(LabelError::InvalidCharset(_))
        ));
    }

    #[test]
    fn exhaustion_fails_closed() {
        let policy = LabelPolicy::new("a", "b").expect("policy");
        let allocator = LabelAllocator::new(policy);
        // Single-char home: left pool "a", right pool "a"; capacity is
        // (1 + 1) * 2 = 4.
        assert_eq!(allocator.capacity(), 4);
        let anchors = points(&[1, 2, 3, 70, 71]);
        assert!(matches!(
            allocator.assign(&anchors, 80),
            Err(LabelError::TooManyTargets { .. })
        ));
    }
}
