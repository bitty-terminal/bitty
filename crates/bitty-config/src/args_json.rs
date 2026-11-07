//! Structural JSON args-blob validation shared by every surface that accepts
//! a raw JSON argument document (CTX-2965, issue #2965).
//!
//! Two surfaces carry a JSON args blob today:
//!
//! - `bitty cmd <qualified-id> -- '<json>'` (the CLI escape hatch);
//! - a keymap/mouse `command:<owner:command> -- '<json>'` action (the
//!   binding surface, including the plugin-command args path).
//!
//! Both pick the blob up as **text** from an untrusted boundary, so both need
//! the same bounded structural check *before* any parser sees it. The check is
//! deliberately structural, not semantic: balanced `{}`/`[]` with string
//! awareness, exactly one root object/array value with no trailing bytes, no
//! unterminated string, and a depth cap. The typed value is produced later by
//! the owning layer's JSON parser
//! (`bitty_runtime::plugin_runtime::store::parse_json`), which never sees a
//! document that failed this gate.
//!
//! One implementation, one set of structural rules. The size bound is per
//! surface: this validator accepts up to [`MAX_JSON_ARGS_BYTES`] for
//! `bitty cmd`, while a keymap binding additionally caps its blob at the
//! smaller [`crate::keymap::MAX_INVOKE_ARGS_BYTES`] before calling here. A
//! binding bound is therefore a strict subset of the CLI's: every document a
//! binding accepts the CLI accepts too, but not the reverse.

/// Maximum bytes for a raw JSON args blob (bounded parser input).
pub const MAX_JSON_ARGS_BYTES: usize = 65_536;

/// Maximum nesting depth accepted in an args blob (wire rule parity: 32).
pub const MAX_JSON_ARGS_DEPTH: usize = 32;

/// Validate a raw JSON args blob structurally.
///
/// # Errors
///
/// Returns a bounded English reason for a blob that is empty, too large, not
/// rooted at an object/array, unbalanced, terminated before its root value
/// closes or carries bytes past the root value (two documents, a trailing
/// token, ...), unterminated inside a string, or nested deeper than
/// [`MAX_JSON_ARGS_DEPTH`]. The caller names its own field or verb in the
/// diagnostic; the reasons here are surface-neutral so every caller can reuse
/// them verbatim.
pub fn validate_json_args_blob(blob: &str) -> Result<(), String> {
    if blob.len() > MAX_JSON_ARGS_BYTES {
        return Err(format!(
            "args blob too large ({} bytes, max {MAX_JSON_ARGS_BYTES})",
            blob.len()
        ));
    }
    let trimmed = blob.trim();
    if trimmed.is_empty() {
        return Err(String::from("args blob must not be empty"));
    }
    let opener = trimmed.as_bytes()[0];
    if opener != b'{' && opener != b'[' {
        return Err(String::from(
            "args blob must be a JSON object or array (e.g. -- '{\"terminal_id\": \"t:4\"}')",
        ));
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut stack: Vec<u8> = Vec::new();
    // Byte offset just past the `}`/`]` that closes the root value. Everything
    // after it must be whitespace: `{"a":1} {"b":2}` and `{"a":1} trailing`
    // are two values / a value plus junk, not one argument document.
    let mut root_end: Option<usize> = None;
    for (index, &b) in trimmed.as_bytes().iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                stack.push(b'}');
                depth += 1;
            }
            b'[' => {
                stack.push(b']');
                depth += 1;
            }
            b'}' | b']' => {
                if stack.pop() != Some(b) {
                    return Err(String::from("args blob has unbalanced brackets"));
                }
                depth = depth.saturating_sub(1);
                if depth == 0 && root_end.is_none() {
                    root_end = Some(index + 1);
                }
            }
            _ => {}
        }
        if depth > MAX_JSON_ARGS_DEPTH {
            return Err(format!(
                "args blob nests too deep (max {MAX_JSON_ARGS_DEPTH})"
            ));
        }
    }
    if in_string {
        return Err(String::from("args blob has an unterminated string"));
    }
    if !stack.is_empty() {
        return Err(String::from("args blob has unbalanced brackets"));
    }
    match root_end {
        Some(end) if trimmed[end..].trim().is_empty() => Ok(()),
        Some(_) => Err(String::from(
            "args blob must hold exactly one root value (bytes follow its closing bracket)",
        )),
        // The opener rule above guarantees a root value; reaching here means
        // the scan never closed it, which the stack check already refused.
        None => Err(String::from("args blob has unbalanced brackets")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_objects_and_arrays() {
        assert!(validate_json_args_blob("{}").is_ok());
        assert!(validate_json_args_blob("[]").is_ok());
        assert!(validate_json_args_blob(r#" {"needle": "a{b]c\", ok", "n": [1, 2]} "#).is_ok());
    }

    #[test]
    fn refuses_non_object_roots_and_empty_blobs() {
        for blob in ["", "   ", "1", "\"text\"", "null"] {
            assert!(
                validate_json_args_blob(blob).is_err(),
                "blob {blob:?} must be refused"
            );
        }
    }

    #[test]
    fn refuses_unbalanced_and_unterminated_blobs() {
        for blob in ["{", "[}", "{\"a\": 1", "{\"a\": \"unterminated}"] {
            let error = validate_json_args_blob(blob).expect_err("must be refused");
            assert!(
                error.contains("unbalanced") || error.contains("unterminated"),
                "blob {blob:?}: {error}"
            );
        }
    }

    #[test]
    fn refuses_more_than_one_root_value_or_trailing_bytes() {
        // `-- {"a":1} {"b":2}` and `-- {"a":1} trailing` are not one argument
        // document; accepting them here would move the refusal from config
        // load to the keypress (the dispatch-side JSON parser rejects them).
        for blob in [
            "{\"a\":1} {\"b\":2}",
            "{\"a\":1} -- {\"b\":2}",
            "{\"a\":1} trailing",
            "{\"a\":1}]",
            "[] []",
            "[1] 2",
        ] {
            let error = validate_json_args_blob(blob).expect_err("must be refused");
            assert!(
                error.contains("exactly one root value") || error.contains("unbalanced"),
                "blob {blob:?}: {error}"
            );
        }
        // One root value with surrounding whitespace is still one document.
        assert!(validate_json_args_blob("  {\"a\":1}\n\t").is_ok());
        assert!(validate_json_args_blob("{}\n").is_ok());
    }

    #[test]
    fn refuses_blobs_past_the_depth_cap() {
        let deep = format!(
            "{}1{}",
            "[".repeat(MAX_JSON_ARGS_DEPTH + 1),
            "]".repeat(MAX_JSON_ARGS_DEPTH + 1)
        );
        let error = validate_json_args_blob(&deep).expect_err("must be refused");
        assert!(error.contains("nests too deep"), "got {error}");
    }

    #[test]
    fn refuses_blobs_past_the_size_cap() {
        let big = format!("{{\"a\": \"{}\"}}", "x".repeat(MAX_JSON_ARGS_BYTES));
        let error = validate_json_args_blob(&big).expect_err("must be refused");
        assert!(error.contains("too large"), "got {error}");
    }
}
