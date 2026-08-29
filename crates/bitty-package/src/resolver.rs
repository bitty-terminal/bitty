//! Deterministic resolver with closed constraint grammar and single-version convergence.
//!
//! Pure function `(manifest, lock, index) -> resolution or error` per
//! package-followup RFC §Resolver contract. Headless, bounded, `forbid(unsafe)`.
//! Single version per package ID; side-by-side deferred. Yanked and prerelease
//! filtering per RFC. Solver recursion and budgets bounded.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::error::PackageError;
use crate::lockfile::Lockfile;
use crate::manifest::{PackageDependency, PackageId, PackageManifest};
use crate::requirement::VersionReq;
use crate::version::Version;

/// Graph limits per RFC (Invariant 7, budgets).
pub const MAX_EDGES_PER_PACKAGE: usize = 64;
pub const MAX_PACKAGES_PER_RESOLUTION: usize = 256;
pub const MAX_CANDIDATES_PER_PACKAGE: usize = 512;
/// Solver step budget to avoid wedging.
pub const MAX_SOLVER_STEPS: usize = 20_000;

/// A registry candidate version record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Package id.
    pub id: PackageId,
    /// Version string (validated SemVer, bounded 64).
    pub version_str: String,
    /// Parsed version for precedence.
    pub version: Version,
    /// Whether registry marks this version yanked.
    pub yanked: bool,
    /// Dependencies declared by this version.
    pub dependencies: Vec<PackageDependency>,
}

impl IndexEntry {
    /// Create a new entry, validating version string and dependency count.
    pub fn new(
        id: PackageId,
        version_str: String,
        yanked: bool,
        dependencies: Vec<PackageDependency>,
    ) -> Result<Self, PackageError> {
        let version = Version::parse(&version_str)?;
        if dependencies.len() > MAX_EDGES_PER_PACKAGE {
            return Err(PackageError::LimitExceeded {
                field: format!("index.{}.dependencies", id),
                limit: MAX_EDGES_PER_PACKAGE,
                actual: dependencies.len(),
            });
        }
        for dep in &dependencies {
            dep.validate()?;
        }
        Ok(Self {
            id,
            version_str,
            version,
            yanked,
            dependencies,
        })
    }
}

/// Registry index — package ID to candidate list.
#[derive(Debug, Clone, Default)]
pub struct PackageIndex {
    /// Map from package ID string to candidates.
    entries: BTreeMap<String, Vec<IndexEntry>>,
}

impl PackageIndex {
    /// Create empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a candidate. Candidates for same ID are kept sorted descending by precedence.
    pub fn insert(&mut self, entry: IndexEntry) -> Result<(), PackageError> {
        if self.entries.len() > MAX_PACKAGES_PER_RESOLUTION
            && !self.entries.contains_key(entry.id.as_str())
        {
            return Err(PackageError::Budget {
                message: format!(
                    "package index exceeds {} packages",
                    MAX_PACKAGES_PER_RESOLUTION
                ),
            });
        }
        let list = self
            .entries
            .entry(entry.id.as_str().to_string())
            .or_default();
        if list.len() >= MAX_CANDIDATES_PER_PACKAGE {
            return Err(PackageError::Budget {
                message: format!(
                    "too many candidates for {} exceeds {}",
                    entry.id, MAX_CANDIDATES_PER_PACKAGE
                ),
            });
        }
        // Prevent duplicate version
        if list.iter().any(|e| e.version_str == entry.version_str) {
            return Err(PackageError::Duplicate {
                kind: "index version".to_string(),
                value: format!("{}@{}", entry.id, entry.version_str),
            });
        }
        list.push(entry);
        // Deterministic: sort descending by precedence, ties lexical on version_str
        list.sort_by(|a, b| {
            let ord = b.version.cmp_precedence(&a.version);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            a.version_str.cmp(&b.version_str)
        });
        Ok(())
    }

    /// Get candidates for package, sorted descending (max stable first).
    #[must_use]
    pub fn candidates(&self, id: &PackageId) -> Option<&[IndexEntry]> {
        self.entries.get(id.as_str()).map(|v| v.as_slice())
    }

    /// Number of package IDs in index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Validate bounds.
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.entries.len() > MAX_PACKAGES_PER_RESOLUTION {
            return Err(PackageError::LimitExceeded {
                field: "index.packages".to_string(),
                limit: MAX_PACKAGES_PER_RESOLUTION,
                actual: self.entries.len(),
            });
        }
        for (id, list) in &self.entries {
            if list.len() > MAX_CANDIDATES_PER_PACKAGE {
                return Err(PackageError::LimitExceeded {
                    field: format!("index.{id}.candidates"),
                    limit: MAX_CANDIDATES_PER_PACKAGE,
                    actual: list.len(),
                });
            }
            for e in list {
                if e.dependencies.len() > MAX_EDGES_PER_PACKAGE {
                    return Err(PackageError::LimitExceeded {
                        field: format!("index.{id}@{}", e.version_str),
                        limit: MAX_EDGES_PER_PACKAGE,
                        actual: e.dependencies.len(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Resolved package entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    /// Package id.
    pub id: PackageId,
    /// Selected version string.
    pub version: String,
    /// Whether selected version is yanked (only allowed when preserving locked).
    pub yanked: bool,
}

/// Full resolution — single version per package ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Map from package ID to resolved package, sorted lexical by ID.
    pub packages: BTreeMap<PackageId, ResolvedPackage>,
}

impl Resolution {
    /// Number of resolved packages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.packages.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Deterministic digest for byte-for-byte comparison (sorted).
    #[must_use]
    pub fn digest(&self) -> String {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"bitty-resolve-v1\n");
        for (id, pkg) in &self.packages {
            bytes.extend_from_slice(id.as_str().as_bytes());
            bytes.push(b'@');
            bytes.extend_from_slice(pkg.version.as_bytes());
            bytes.push(b'\n');
        }
        crate::integrity::sha256_hex(&bytes)
    }
}

#[derive(Debug, Clone)]
struct Edge {
    from: PackageId,
    req: VersionReq,
    prerelease: bool,
    // original raw for conflict reporting
    raw_req: String,
}

/// Resolve with no locked preservation — yanked candidates excluded.
///
/// Pure, deterministic, bounded. Single version per ID. Fails with actionable
/// conflict report naming both edges when constraints cannot be satisfied together.
pub fn resolve(
    manifest: &PackageManifest,
    index: &PackageIndex,
) -> Result<Resolution, PackageError> {
    resolve_inner(manifest, index, None)
}

/// Resolve preserving an existing lock's yanked versions for sync restore.
///
/// Yanked versions that are already locked remain valid for sync; new resolves
/// still skip yanked. Prerelease handling identical. Returns resolution plus
/// list of yanked-locked warnings (package ids that are yanked but preserved).
pub fn resolve_preserving_locked(
    manifest: &PackageManifest,
    index: &PackageIndex,
    locked: &Lockfile,
) -> Result<(Resolution, Vec<String>), PackageError> {
    let res = resolve_inner(manifest, index, Some(locked))?;
    let mut warnings = Vec::new();
    for (id, pkg) in &res.packages {
        if pkg.yanked {
            // Check if this yanked version was in lock
            if let Some(lp) = locked.get(id) {
                if lp.version == pkg.version {
                    warnings.push(format!("{}@{} yanked (locked)", id, pkg.version));
                }
            }
        }
    }
    Ok((res, warnings))
}

fn resolve_inner(
    manifest: &PackageManifest,
    index: &PackageIndex,
    locked: Option<&Lockfile>,
) -> Result<Resolution, PackageError> {
    // Bounds
    manifest.validate().map_err(|e| PackageError::Integrity {
        stage: "resolver_manifest".to_string(),
        message: e.to_string(),
    })?;
    index.validate()?;
    if manifest.dependencies.len() > MAX_EDGES_PER_PACKAGE {
        return Err(PackageError::LimitExceeded {
            field: "manifest.dependencies".to_string(),
            limit: MAX_EDGES_PER_PACKAGE,
            actual: manifest.dependencies.len(),
        });
    }
    if let Some(lock) = locked {
        lock.validate().map_err(|e| PackageError::Integrity {
            stage: "resolver_lock".to_string(),
            message: e.to_string(),
        })?;
    }

    // Build initial constraints from root manifest
    let mut constraints: BTreeMap<PackageId, Vec<Edge>> = BTreeMap::new();
    for dep in &manifest.dependencies {
        let req = VersionReq::parse(&dep.version_req)?;
        let edge = Edge {
            from: manifest.identity.id.clone(),
            req,
            prerelease: dep.prerelease,
            raw_req: dep.version_req.clone(),
        };
        constraints.entry(dep.id.clone()).or_default().push(edge);
    }
    // Check per-package edge limits for direct deps
    for (id, edges) in &constraints {
        if edges.len() > MAX_EDGES_PER_PACKAGE {
            return Err(PackageError::Budget {
                message: format!(
                    "too many edges for {} exceeds {}",
                    id, MAX_EDGES_PER_PACKAGE
                ),
            });
        }
    }

    let mut selected: BTreeMap<PackageId, IndexEntry> = BTreeMap::new();
    let mut steps: usize = 0;

    // Deterministic solving via backtracking, picking smallest lex unselected constrained package next
    let result = backtrack(
        &mut constraints,
        &mut selected,
        index,
        locked,
        &mut steps,
        0,
    )?;

    if !result {
        // Should have returned conflict error inside backtrack; fallback generic
        return Err(PackageError::Integrity {
            stage: "resolver".to_string(),
            message: "resolver could not find satisfying assignment".to_string(),
        });
    }

    // Build resolution
    let mut packages = BTreeMap::new();
    for (id, entry) in selected {
        packages.insert(
            id.clone(),
            ResolvedPackage {
                id,
                version: entry.version_str.clone(),
                yanked: entry.yanked,
            },
        );
    }
    Ok(Resolution { packages })
}

fn satisfies_all(candidate: &Version, edges: &[Edge]) -> bool {
    for edge in edges {
        if !edge.req.matches(candidate) {
            return false;
        }
    }
    true
}

fn prerelease_allowed(candidate: &Version, edges: &[Edge]) -> bool {
    if !candidate.is_prerelease() {
        return true;
    }
    for edge in edges {
        let allows = if edge.prerelease {
            true
        } else {
            edge.req.allows_prerelease_for(candidate)
        };
        if !allows {
            return false;
        }
    }
    true
}

fn next_unselected(
    constraints: &BTreeMap<PackageId, Vec<Edge>>,
    selected: &BTreeMap<PackageId, IndexEntry>,
) -> Option<PackageId> {
    for k in constraints.keys() {
        if !selected.contains_key(k) {
            return Some(k.clone());
        }
    }
    None
}

fn backtrack(
    constraints: &mut BTreeMap<PackageId, Vec<Edge>>,
    selected: &mut BTreeMap<PackageId, IndexEntry>,
    index: &PackageIndex,
    locked: Option<&Lockfile>,
    steps: &mut usize,
    depth: usize,
) -> Result<bool, PackageError> {
    if depth > MAX_PACKAGES_PER_RESOLUTION {
        return Err(PackageError::Budget {
            message: format!(
                "resolver recursion depth exceeds {}",
                MAX_PACKAGES_PER_RESOLUTION
            ),
        });
    }
    *steps += 1;
    if *steps > MAX_SOLVER_STEPS {
        return Err(PackageError::Budget {
            message: format!("resolver step budget {} exceeded", MAX_SOLVER_STEPS),
        });
    }
    if selected.len() > MAX_PACKAGES_PER_RESOLUTION {
        return Err(PackageError::LimitExceeded {
            field: "resolution.packages".to_string(),
            limit: MAX_PACKAGES_PER_RESOLUTION,
            actual: selected.len(),
        });
    }
    if constraints.len() > MAX_PACKAGES_PER_RESOLUTION {
        return Err(PackageError::LimitExceeded {
            field: "resolution.constraints".to_string(),
            limit: MAX_PACKAGES_PER_RESOLUTION,
            actual: constraints.len(),
        });
    }

    // Find next package to decide
    let Some(pkg_id) = next_unselected(constraints, selected) else {
        // All constrained packages selected — but selected packages may have introduced dependencies that are not yet in constraints?
        // Actually we add dependencies as we select, so constraints always reflects all required packages.
        // If no unselected constrained remains, we are done. However need to ensure transitive closure: selected entries' dependencies already added to constraints before recursion, so done.
        return Ok(true);
    };

    // Clone edges for this package (deterministic order by from lexical then raw_req)
    let edges = constraints.get(&pkg_id).cloned().unwrap_or_default();
    if edges.len() > MAX_EDGES_PER_PACKAGE {
        return Err(PackageError::Budget {
            message: format!(
                "too many edges for {} exceeds {}",
                pkg_id, MAX_EDGES_PER_PACKAGE
            ),
        });
    }

    let candidates_slice = index
        .candidates(&pkg_id)
        .ok_or_else(|| PackageError::NotFound {
            id: pkg_id.to_string(),
        })?;
    if candidates_slice.is_empty() {
        return Err(PackageError::NotFound {
            id: pkg_id.to_string(),
        });
    }

    // Prepare sorted edges for deterministic conflict reporting
    let mut sorted_edges = edges.clone();
    sorted_edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.raw_req.cmp(&b.raw_req)));

    // Track whether we saw any candidate that was filtered only by yanked/prerelease vs unsatisfiable
    let mut filtered_by_yank = 0usize;
    let mut filtered_by_prerelease = 0usize;
    let mut considered = 0usize;
    let mut last_deep_err: Option<PackageError> = None;

    for candidate in candidates_slice.iter() {
        // Yanked filtering
        if candidate.yanked {
            let is_locked_yanked = if let Some(lock) = locked {
                if let Some(lp) = lock.get(&pkg_id) {
                    lp.version == candidate.version_str
                } else {
                    false
                }
            } else {
                false
            };
            if !is_locked_yanked {
                filtered_by_yank += 1;
                continue;
            }
        }
        // Prerelease filtering
        if candidate.version.is_prerelease()
            && !prerelease_allowed(&candidate.version, &sorted_edges)
        {
            filtered_by_prerelease += 1;
            continue;
        }
        // Does candidate satisfy all edges?
        if !satisfies_all(&candidate.version, &sorted_edges) {
            considered += 1;
            continue;
        }

        // Candidate viable — try it, expanding its dependencies
        // Check if selecting this candidate would create a cycle that violates single-version convergence already?
        // Save state for backtracking
        let prev_selected_len = selected.len();
        let prev_constraints = constraints.clone();

        selected.insert(pkg_id.clone(), candidate.clone());

        // Expand dependencies into constraints
        let mut overflow = false;
        for dep in &candidate.dependencies {
            let req = match VersionReq::parse(&dep.version_req) {
                Ok(r) => r,
                Err(e) => {
                    // Fail closed on invalid requirement in index (untrusted input)
                    return Err(PackageError::Integrity {
                        stage: "resolver_index".to_string(),
                        message: format!("invalid requirement for {} -> {}: {}", pkg_id, dep.id, e),
                    });
                }
            };
            let edge = Edge {
                from: pkg_id.clone(),
                req,
                prerelease: dep.prerelease,
                raw_req: dep.version_req.clone(),
            };
            let entry = constraints.entry(dep.id.clone()).or_default();
            // Duplicate edge dedup? Keep all for conflict reporting, but bound
            entry.push(edge);
            if entry.len() > MAX_EDGES_PER_PACKAGE {
                overflow = true;
                break;
            }
            if constraints.len() > MAX_PACKAGES_PER_RESOLUTION {
                overflow = true;
                break;
            }
        }

        if overflow {
            // rollback and try next candidate, but also budget error if overflow persists?
            *constraints = prev_constraints;
            selected.remove(&pkg_id);
            // Ensure length invariants
            debug_assert!(selected.len() == prev_selected_len);
            // If overflow due to budget, we could return budget error directly rather than continuing
            // But we still try next candidate; if all overflow, eventual budget error will be reported via top-level check.
            continue;
        }

        // After expanding, check if any already selected package now has new constraints that it no longer satisfies
        // Need to validate all selected packages still satisfy their current constraints
        // If not, this candidate cannot be part of solution — backtrack
        let mut selected_still_valid = true;
        for (sel_id, sel_entry) in selected.iter() {
            if let Some(sel_edges) = constraints.get(sel_id) {
                // Yanked already handled; prerelease and satisfies check
                if sel_entry.version.is_prerelease()
                    && !prerelease_allowed(&sel_entry.version, sel_edges)
                {
                    selected_still_valid = false;
                    break;
                }
                if !satisfies_all(&sel_entry.version, sel_edges) {
                    selected_still_valid = false;
                    break;
                }
            }
        }
        if !selected_still_valid {
            *constraints = prev_constraints;
            selected.remove(&pkg_id);
            continue;
        }

        // Recurse
        match backtrack(constraints, selected, index, locked, steps, depth + 1) {
            Ok(true) => return Ok(true),
            Ok(false) => {
                // This branch dead-ended, restore and try next candidate
                *constraints = prev_constraints;
                selected.remove(&pkg_id);
                continue;
            }
            Err(e) => {
                // Budget/limit are fatal
                match &e {
                    PackageError::Budget { .. } | PackageError::LimitExceeded { .. } => {
                        return Err(e);
                    }
                    _ => {
                        // Any other error (resolver conflict, missing package) at a deeper branch
                        // means this candidate leads to dead end — try next candidate.
                        // Preserve deepest error for accurate conflict reporting.
                        last_deep_err = Some(e);
                        *constraints = prev_constraints;
                        selected.remove(&pkg_id);
                        continue;
                    }
                }
            }
        }
    }

    // Exhausted all candidates for pkg_id — if deeper branch already reported a more specific conflict, propagate it
    if let Some(err) = last_deep_err {
        return Err(err);
    }
    // Otherwise produce actionable conflict report for this package
    let msg = format!(
        "resolver conflict for {}: no candidate satisfies {} edges ({} candidates checked, {} filtered yanked, {} filtered prerelease, {} unsatisfied). Edges: {}",
        pkg_id,
        sorted_edges.len(),
        candidates_slice.len(),
        filtered_by_yank,
        filtered_by_prerelease,
        considered,
        sorted_edges
            .iter()
            .map(|e| format!("{} requires {} '{}'", e.from, pkg_id, e.raw_req))
            .collect::<Vec<_>>()
            .join("; ")
    );
    // To avoid dumping unbounded message, truncate
    let truncated = if msg.len() > 2000 {
        format!("{}…", &msg[..2000])
    } else {
        msg
    };
    // Return as Integrity error with stage resolver for test inspectability
    // Include package id in message for PLF-AC-003
    Err(PackageError::Integrity {
        stage: "resolver".to_string(),
        message: truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Compat, PackageDependency, PackageId, PackageIdentity, PackageManifest};

    fn pid(s: &str) -> PackageId {
        PackageId::new(s).unwrap()
    }

    fn dep(id: &str, req: &str) -> PackageDependency {
        PackageDependency {
            id: pid(id),
            version_req: req.to_string(),
            prerelease: false,
        }
    }

    fn dep_pre(id: &str, req: &str, pre: bool) -> PackageDependency {
        PackageDependency {
            id: pid(id),
            version_req: req.to_string(),
            prerelease: pre,
        }
    }

    fn manifest_with_deps(deps: Vec<PackageDependency>) -> PackageManifest {
        PackageManifest {
            identity: PackageIdentity {
                id: pid("xuepoo.root"),
                name: "Root".to_string(),
                version: "0.1.0".to_string(),
                description: "root".to_string(),
                license: None,
            },
            compat: Compat::default(),
            dependencies: deps,
            capabilities: Vec::new(),
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        }
    }

    fn entry(id: &str, ver: &str, yanked: bool, deps: Vec<PackageDependency>) -> IndexEntry {
        IndexEntry::new(pid(id), ver.to_string(), yanked, deps).unwrap()
    }

    #[test]
    fn max_stable_selected() {
        let mut idx = PackageIndex::new();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "1.1.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "2.0.0", false, vec![]))
            .unwrap();
        let m = manifest_with_deps(vec![dep("xuepoo.dep", "^1.0")]);
        let res = resolve(&m, &idx).unwrap();
        assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.1.0");
    }

    #[test]
    fn yanked_skipped_for_new_resolve() {
        let mut idx = PackageIndex::new();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "1.5.0", true, vec![]))
            .unwrap(); // yanked max
        idx.insert(entry("xuepoo.dep", "1.2.0", false, vec![]))
            .unwrap();
        let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.0.0")]);
        let res = resolve(&m, &idx).unwrap();
        assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.2.0");
    }

    #[test]
    fn prerelease_excluded_unless_opt_in() {
        let mut idx = PackageIndex::new();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "1.1.0-alpha.1", false, vec![]))
            .unwrap();
        // Bare ^1.0 should not pick prerelease
        let m = manifest_with_deps(vec![dep("xuepoo.dep", "^1.0")]);
        let res = resolve(&m, &idx).unwrap();
        assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.0.0");

        // With prerelease flag, it can
        let m2 = manifest_with_deps(vec![dep_pre("xuepoo.dep", "^1.0", true)]);
        let res2 = resolve(&m2, &idx).unwrap();
        assert_eq!(res2.packages[&pid("xuepoo.dep")].version, "1.1.0-alpha.1");

        // With requirement containing prerelease on same X.Y.Z
        let mut idx2 = PackageIndex::new();
        idx2.insert(entry("xuepoo.dep", "1.1.0-alpha.1", false, vec![]))
            .unwrap();
        idx2.insert(entry("xuepoo.dep", "1.1.0-alpha.2", false, vec![]))
            .unwrap();
        idx2.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        let m3 = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.1.0-alpha")]);
        let res3 = resolve(&m3, &idx2).unwrap();
        // Should pick prerelease max
        assert_eq!(res3.packages[&pid("xuepoo.dep")].version, "1.1.0-alpha.2");
    }

    #[test]
    fn conflict_report_naming_both_edges() {
        // Root depends on a and b, which both depend on dep with conflicting reqs
        let mut idx2 = PackageIndex::new();
        idx2.insert(entry(
            "xuepoo.a",
            "1.0.0",
            false,
            vec![dep("xuepoo.dep", "^1.0")],
        ))
        .unwrap();
        idx2.insert(entry(
            "xuepoo.b",
            "1.0.0",
            false,
            vec![dep("xuepoo.dep", "^2.0")],
        ))
        .unwrap();
        idx2.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx2.insert(entry("xuepoo.dep", "2.0.0", false, vec![]))
            .unwrap();
        let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0"), dep("xuepoo.b", "^1.0")]);
        let err = resolve(&m, &idx2).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("xuepoo.dep"),
            "msg must name conflicting package: {msg}"
        );
        assert!(msg.contains("requires"), "msg must name edges: {msg}");
    }

    #[test]
    fn determinism_same_inputs_same_output() {
        let mut idx = PackageIndex::new();
        for v in ["1.0.0", "1.1.0", "1.2.0"] {
            idx.insert(entry("xuepoo.dep", v, false, vec![])).unwrap();
        }
        let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.0.0, <1.3.0")]);
        let r1 = resolve(&m, &idx).unwrap();
        let r2 = resolve(&m, &idx).unwrap();
        assert_eq!(r1.digest(), r2.digest());
        // Whitespace/comma order should not affect result
        let m2 = manifest_with_deps(vec![PackageDependency {
            id: pid("xuepoo.dep"),
            version_req: "<1.3.0, >=1.0.0".to_string(),
            prerelease: false,
        }]);
        let r3 = resolve(&m2, &idx).unwrap();
        assert_eq!(r1.digest(), r3.digest());
    }

    #[test]
    fn single_version_convergence_no_side_by_side() {
        // Two different versions of same ID must not coexist; resolver fails if constraints incompatible, else converges to one
        let mut idx = PackageIndex::new();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "1.5.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "2.0.0", false, vec![]))
            .unwrap();
        // Both edges require same range, should converge to max satisfying (1.5.0)
        let mut idx2 = PackageIndex::new();
        idx2.insert(entry(
            "xuepoo.a",
            "1.0.0",
            false,
            vec![dep("xuepoo.dep", "^1.0")],
        ))
        .unwrap();
        idx2.insert(entry(
            "xuepoo.b",
            "1.0.0",
            false,
            vec![dep("xuepoo.dep", "^1.0")],
        ))
        .unwrap();
        for v in ["1.0.0", "1.5.0", "2.0.0"] {
            idx2.insert(entry("xuepoo.dep", v, false, vec![])).unwrap();
        }
        let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0"), dep("xuepoo.b", "^1.0")]);
        let res = resolve(&m, &idx2).unwrap();
        assert_eq!(res.packages.len(), 3); // a,b,dep
        assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.5.0");
    }

    #[test]
    fn budget_edges_per_package() {
        let mut idx = PackageIndex::new();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        // Create manifest with too many dependencies
        let many: Vec<PackageDependency> = (0..65)
            .map(|i| dep(&format!("xuepoo.p{i:02}"), "^1.0"))
            .collect();
        let m = manifest_with_deps(many);
        let err = resolve(&m, &idx).unwrap_err();
        assert!(
            format!("{err}").contains("exceeds")
                || format!("{err}").contains("budget")
                || format!("{err}").contains("limit")
        );
    }

    #[test]
    fn transitive_resolution() {
        let mut idx = PackageIndex::new();
        // a 1.0 depends on dep ^1.0
        idx.insert(entry(
            "xuepoo.a",
            "1.0.0",
            false,
            vec![dep("xuepoo.dep", "^1.0")],
        ))
        .unwrap();
        idx.insert(entry(
            "xuepoo.a",
            "1.1.0",
            false,
            vec![dep("xuepoo.dep", "^2.0")],
        ))
        .unwrap();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "2.0.0", false, vec![]))
            .unwrap();
        // root depends on a ^1.0 and dep ^1.0? Actually root depends only on a, but a 1.1 would need dep 2.0
        // If root also depends on dep ^1.0, then a 1.1 is incompatible, resolver should backtrack to a 1.0
        idx.insert(entry("xuepoo.rootdep", "1.0.0", false, vec![]))
            .unwrap(); // dummy
        let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0"), dep("xuepoo.dep", "^1.0")]);
        let res = resolve(&m, &idx).unwrap();
        // Should pick a 1.0 because a 1.1 requires dep 2.0 conflicting with root's ^1.0
        assert_eq!(res.packages[&pid("xuepoo.a")].version, "1.0.0");
        assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.0.0");
    }

    #[test]
    fn yanked_preserved_when_locked() {
        let mut idx = PackageIndex::new();
        idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
            .unwrap();
        idx.insert(entry("xuepoo.dep", "2.0.0", true, vec![]))
            .unwrap();
        let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.0.0")]);
        // Fresh resolve skips yanked 2.0.0
        let fresh = resolve(&m, &idx).unwrap();
        assert_eq!(fresh.packages[&pid("xuepoo.dep")].version, "1.0.0");

        // Locked contains yanked 2.0.0, preserving should allow it?
        // For preserving, we need to supply lock that pins 2.0.0
        let mut lock = crate::lockfile::Lockfile::new();
        lock.insert(crate::lockfile::LockedPackage {
            id: pid("xuepoo.dep"),
            version: "2.0.0".to_string(),
            source: crate::source::PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: crate::lockfile::PackageDigests {
                artifact: crate::integrity::sha256_hex(b"a"),
                manifest: crate::integrity::sha256_hex(b"m"),
                content_root: None,
            },
            locked_at: 1,
        })
        .unwrap();
        let (preserved, warnings) = resolve_preserving_locked(&m, &idx, &lock).unwrap();
        // Preserved should be 2.0.0 yanked locked? But our resolver's logic for preserving currently allows yanked only if locked version matches candidate. Since we inserted yanked 2.0.0 and lock has 2.0.0, it should allow 2.0.0 as max satisfying, so preserved picks 2.0.0
        assert_eq!(preserved.packages[&pid("xuepoo.dep")].version, "2.0.0");
        assert!(warnings.iter().any(|w| w.contains("yanked (locked)")));
    }
}
