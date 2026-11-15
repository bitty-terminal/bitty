//! P0-AC-022 (bitty#2921): the full 15-scope by 51-method authorization matrix
//! as one runnable artifact.
//!
//! `P0-AC-022` asks that an authenticated client granted only inspect/read
//! scope is denied every write-style action, and its pass threshold is a "full
//! scope x action matrix shows denial outside grants". The daemon's own
//! `session.*`/`daemon.*` catalog is 20 methods over 6 service scopes and is
//! exercised live by the conformance corpus (`C-09`, `C-17`, `C-25`..`C-28`).
//! The v1 transport matrix the criterion's wording names is the `bitty-ipc-api`
//! registry: 51 frozen method names over 15 scopes, and its single decision
//! point is `authorize_method`.
//!
//! This target walks every one of the 15 x 51 = 765 cells and asserts the
//! matrix instead of sampling it:
//!
//! - the empty grant set denies all 51 methods with `ScopeDenied`, naming that
//!   method's own required scope and never a different one;
//! - granting exactly one scope allows exactly the methods that require it and
//!   denies the rest with `ScopeDenied`, so no scope is a substitute for
//!   another;
//! - the grid has 51 allowed cells, one per row, which is what "denial outside
//!   grants" means as a count;
//! - the all-scopes set allows every method, so the denials above are caused by
//!   the missing scope and not by an unrelated stage;
//! - an unknown method is `NotFound` and a malformed name is `InvalidMethod`
//!   even with every scope held, so deny-by-default does not depend on a well
//!   formed name.
//!
//! Run with `cargo test -p bitty-daemon --test scope_matrix -- --nocapture` and
//! the grid is printed between the `SCOPE_MATRIX_GRID` markers, which is how the
//! dated evidence record's appendix is produced. The printed table is the
//! artifact; the assertions are what make it trustworthy.

use bitty_ipc_api::IpcError;
use bitty_ipc_api::scope::{
    Scope, ScopeSet, all_known_methods, authorize_method, required_scope_for_method,
};

/// A grant set holding exactly one scope.
fn single(scope: Scope) -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(scope);
    set
}

/// The `ScopeDenied` scope name a refusal carries, when it is a scope refusal.
fn denied_scope(error: &IpcError) -> Option<&str> {
    match error {
        IpcError::ScopeDenied { scope, .. } => Some(scope.as_str()),
        _ => None,
    }
}

/// One cell of the matrix, as the decision point reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    /// The grant set satisfies the method's requirement.
    Allowed,
    /// The grant set lacks the method's required scope.
    ScopeDenied,
    /// Anything else: a registry or grammar refusal.
    Other,
}

/// Evaluate one (method, grant set) cell without panicking.
fn cell(method: &str, granted: &ScopeSet) -> Cell {
    match authorize_method(method, granted) {
        Ok(_) => Cell::Allowed,
        Err(error) => {
            if denied_scope(&error).is_some() {
                Cell::ScopeDenied
            } else {
                Cell::Other
            }
        }
    }
}

#[test]
fn every_method_declares_a_scope_and_the_registry_is_fifty_one_rows() {
    let methods = all_known_methods();
    assert_eq!(methods.len(), 51, "the v1 registry changed size");
    assert_eq!(Scope::all().len(), 15, "the v1 scope set changed size");
    for method in methods {
        let required = required_scope_for_method(method);
        assert!(
            required.is_some(),
            "{method} has no required scope, so it would be deny-by-default by accident"
        );
    }
}

#[test]
fn the_empty_grant_set_denies_every_method_with_its_own_scope() {
    let empty = ScopeSet::new();
    let mut denied = 0_usize;
    for method in all_known_methods() {
        let required = required_scope_for_method(method)
            .unwrap_or_else(|| panic!("{method} has no required scope"));
        let error =
            authorize_method(method, &empty).expect_err("no granted scope must deny every method");
        assert_eq!(
            denied_scope(&error),
            Some(required.as_str()),
            "{method} must name its own required scope"
        );
        match &error {
            IpcError::ScopeDenied { action, .. } => {
                assert_eq!(action, method, "{method} must name itself as the action");
            }
            other => panic!("{method} must be ScopeDenied, got {other:?}"),
        }
        denied += 1;
    }
    assert_eq!(denied, 51, "every method is denied with no grant");
}

#[test]
fn a_single_scope_allows_only_the_methods_that_require_it() {
    let mut allowed_cells = 0_usize;
    let mut denied_cells = 0_usize;
    for scope in Scope::all() {
        let granted = single(*scope);
        for method in all_known_methods() {
            let required = required_scope_for_method(method)
                .unwrap_or_else(|| panic!("{method} has no required scope"));
            match cell(method, &granted) {
                Cell::Allowed => {
                    assert_eq!(
                        required, *scope,
                        "{method} was allowed by {scope} but requires {required}"
                    );
                    allowed_cells += 1;
                }
                Cell::ScopeDenied => {
                    assert_ne!(
                        required, *scope,
                        "{method} requires {required} and holds it, so it cannot be denied"
                    );
                    let error = authorize_method(method, &granted)
                        .expect_err("a denied cell carries an error");
                    assert_eq!(
                        denied_scope(&error),
                        Some(required.as_str()),
                        "{method} must be denied for its own scope, never for {scope}"
                    );
                    denied_cells += 1;
                }
                Cell::Other => panic!("{method} under {scope} refused outside the scope stage"),
            }
        }
    }
    assert_eq!(
        allowed_cells, 51,
        "the grid must hold exactly one allowed cell per method"
    );
    assert_eq!(
        allowed_cells + denied_cells,
        15 * 51,
        "every cell of the 15 x 51 grid is decided"
    );
    assert_eq!(denied_cells, 14 * 51, "every non-own scope is denied");
}

#[test]
fn no_scope_is_a_substitute_for_another() {
    for method in all_known_methods() {
        let required = required_scope_for_method(method)
            .unwrap_or_else(|| panic!("{method} has no required scope"));
        for scope in Scope::all() {
            if *scope == required {
                continue;
            }
            assert_eq!(
                cell(method, &single(*scope)),
                Cell::ScopeDenied,
                "{method} must not be allowed by the unrelated scope {scope}"
            );
        }
    }
}

#[test]
fn holding_every_scope_allows_every_method() {
    let all = ScopeSet::all();
    for method in all_known_methods() {
        assert_eq!(
            cell(method, &all),
            Cell::Allowed,
            "{method} must be allowed when its own scope is held"
        );
    }
}

#[test]
fn unknown_and_malformed_names_are_refused_with_every_scope_held() {
    let all = ScopeSet::all();
    let unknown = authorize_method("session.unknown", &all).expect_err("unknown method");
    assert!(
        matches!(unknown, IpcError::NotFound { .. }),
        "an unknown method is NotFound, got {unknown:?}"
    );
    // Every v1 family is closed, so a new word inside one is not a method.
    let family_unknown =
        authorize_method("terminal.teleport", &all).expect_err("unknown family member");
    assert!(
        matches!(family_unknown, IpcError::NotFound { .. }),
        "an unknown family member is NotFound, got {family_unknown:?}"
    );
    let malformed = authorize_method("Terminal.List", &all).expect_err("malformed method");
    assert!(
        matches!(malformed, IpcError::InvalidMethod { .. }),
        "a malformed name is InvalidMethod, got {malformed:?}"
    );
}

#[test]
fn the_scope_by_method_grid_is_printed_as_the_artifact() {
    let scopes = Scope::all();
    let methods = all_known_methods();
    let mut out = String::new();
    out.push_str("SCOPE_MATRIX_GRID_BEGIN\n");
    out.push_str("| method | required scope |");
    for scope in scopes {
        out.push(' ');
        out.push_str(scope.as_str());
        out.push_str(" |");
    }
    out.push('\n');
    out.push_str("| --- | --- |");
    for _ in scopes {
        out.push_str(" --- |");
    }
    out.push('\n');
    for method in methods {
        let required = required_scope_for_method(method)
            .unwrap_or_else(|| panic!("{method} has no required scope"));
        out.push_str("| `");
        out.push_str(method);
        out.push_str("` | `");
        out.push_str(required.as_str());
        out.push_str("` |");
        for scope in scopes {
            out.push_str(match cell(method, &single(*scope)) {
                Cell::Allowed => " ok |",
                Cell::ScopeDenied => " — |",
                Cell::Other => " ? |",
            });
        }
        out.push('\n');
    }
    out.push_str("SCOPE_MATRIX_GRID_END\n");
    print!("{out}");
    // The legend and the counts the appendix cites, printed with the grid so a
    // reviewer never has to recompute them from the table.
    println!(
        "legend: ok = allowed; — = Denied/ScopeDenied(class `scope`, code `ScopeDenied`); \
         cells = {}, allowed = {}, denied = {}",
        scopes.len() * methods.len(),
        51,
        14 * 51
    );
}
