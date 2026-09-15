//! Bounded reader for the `bitty-plugin.toml` manifest subset.
//!
//! The full package-integrity pipeline (signatures, artifact verification, the
//! complete TOML schema) stays with the package manager; the runtime's Gap B
//! resolution re-verifies the parsed manifest's `manifest_hash` and the module
//! tree's `content_digest` against the stored record. This module is the
//! minimal, fail-closed reader used for both, and performs no I/O: the caller
//! supplies the already-bounded bytes.
//!
//! Supported sections: `[plugin]`, `[compat]`, `[capabilities]`, `[lazy]`,
//! `[tools.git]` (accepted Layer-2 v1, CTX-0425), and
//! `[[capabilities.filesystem]]` (array-of-tables). Strings, booleans, and
//! arrays of strings are supported. Unknown sections, sub-tables, numeric
//! values, and duplicate keys fail closed.

use std::collections::BTreeSet;

use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::manifest::{
    ACCEPTED_TOOLS, CapabilityRequests, Compat, FilesystemRequest, FsAccess, LazyTriggers,
    PluginIdentity, PluginManifest, QualifiedName, ToolDeclaration,
};

/// Parse a bounded `bitty-plugin.toml` body.
///
/// # Errors
///
/// Returns a bounded message when the body is over the 256 KiB ceiling, is not
/// UTF-8, or violates the supported schema.
pub fn parse_manifest(bytes: &[u8]) -> Result<PluginManifest, String> {
    if bytes.len() > bitty_plugin_host::manifest::MANIFEST_MAX_BYTES {
        return Err("manifest exceeds the 256 KiB ceiling".to_string());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "manifest must be valid UTF-8")?;

    let mut section = String::new();
    let mut id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut description = String::new();
    let mut license: Option<String> = None;
    let mut compat_bitty: Option<String> = None;
    let mut compat_api: Option<String> = None;
    let mut capabilities: BTreeSet<CapabilityId> = BTreeSet::new();
    let mut lazy_commands: Vec<String> = Vec::new();
    let mut lazy_events: Vec<String> = Vec::new();
    let mut lazy_claims: Vec<String> = Vec::new();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();

    // `[[capabilities.filesystem]]` array entries.
    let mut filesystem: Vec<FilesystemRequest> = Vec::new();
    let mut fs_access: Option<String> = None;
    let mut fs_paths: Option<Vec<String>> = None;
    let mut fs_index: usize = 0;
    let mut fs_entry_active = false;

    // `[tools.git]` declaration (accepted v1 only).
    let mut tools_git_seen = false;
    let mut tools_git_required: Option<bool> = None;
    let mut tools_git_version: Option<String> = None;

    let flush_filesystem_entry = |fs_access: &mut Option<String>,
                                  fs_paths: &mut Option<Vec<String>>,
                                  filesystem: &mut Vec<FilesystemRequest>,
                                  fs_entry_active: &mut bool|
     -> Result<(), String> {
        if !*fs_entry_active {
            return Ok(());
        }
        let access_raw = fs_access.take().ok_or(
            "[[capabilities.filesystem]] entry is missing 'access = \"read\"|\"write\"'"
                .to_string(),
        )?;
        let paths = fs_paths
            .take()
            .ok_or("[[capabilities.filesystem]] entry is missing 'paths = [...]'".to_string())?;
        let access = match access_raw.as_str() {
            "read" => FsAccess::Read,
            "write" => FsAccess::Write,
            _ => {
                return Err(format!(
                    "unsupported filesystem access '{access_raw}' (expected \"read\" or \"write\")"
                ));
            }
        };
        filesystem.push(FilesystemRequest { access, paths });
        *fs_entry_active = false;
        Ok(())
    };

    let mut lines = text.lines().enumerate().peekable();
    while let Some((line_number, raw_line)) = lines.next() {
        let line = strip_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // Array-of-tables `[[...]]` (only `[[capabilities.filesystem]]`).
            if line.starts_with("[[") && line.ends_with("]]") {
                let inner = line[2..line.len() - 2].trim().to_string();
                if inner != "capabilities.filesystem" {
                    return Err(format!(
                        "unsupported manifest section '[[{inner}]]' at line {}",
                        line_number + 1
                    ));
                }
                flush_filesystem_entry(
                    &mut fs_access,
                    &mut fs_paths,
                    &mut filesystem,
                    &mut fs_entry_active,
                )?;
                fs_entry_active = true;
                // Index for dedupe is the count of already-flushed entries.
                fs_index = filesystem.len();
                section = "capabilities.filesystem".to_string();
                continue;
            }
            // Single-table `[...]`.
            let inner = line[1..line.len() - 1].trim().to_string();
            // Reject single-bracket filesystem (must be `[[...]]` array).
            if inner == "capabilities.filesystem" {
                return Err(format!(
                    "unsupported manifest section '[{inner}]' at line {} (expected '[[capabilities.filesystem]]')",
                    line_number + 1
                ));
            }
            if let Some(tool) = inner.strip_prefix("tools.") {
                // Only the accepted `[tools.git]` slice (CTX-0425 v1).
                if !ACCEPTED_TOOLS.contains(&tool) {
                    return Err(format!(
                        "unsupported manifest section '[{inner}]' at line {} (only [tools.git] is accepted)",
                        line_number + 1
                    ));
                }
                // Flush any open filesystem entry before switching sections.
                flush_filesystem_entry(
                    &mut fs_access,
                    &mut fs_paths,
                    &mut filesystem,
                    &mut fs_entry_active,
                )?;
                tools_git_seen = true;
                section = inner;
                continue;
            }
            match inner.as_str() {
                "plugin" | "compat" | "capabilities" | "lazy" => {
                    flush_filesystem_entry(
                        &mut fs_access,
                        &mut fs_paths,
                        &mut filesystem,
                        &mut fs_entry_active,
                    )?;
                    section = inner;
                }
                "tools" => {
                    return Err(format!(
                        "unsupported manifest section '[tools]' at line {} (expected '[tools.git]')",
                        line_number + 1
                    ));
                }
                other => {
                    return Err(format!(
                        "unsupported manifest section '[{other}]' at line {}",
                        line_number + 1
                    ));
                }
            }
            continue;
        }
        let (raw_key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("expected 'key = value' at line {}", line_number + 1))?;
        let key =
            parse_key(raw_key.trim()).map_err(|e| format!("{e} at line {}", line_number + 1))?;
        let mut value = value.trim().to_string();

        // Multi-line arrays: keep consuming until the bracket closes.
        if value.starts_with('[') && !balanced_brackets(&value) {
            for (next_number, next_raw) in lines.by_ref() {
                let next = strip_comment(next_raw).trim().to_string();
                value.push(' ');
                value.push_str(&next);
                if balanced_brackets(&value) {
                    break;
                }
                if next_number > line_number + 4096 {
                    return Err("manifest array exceeded the line ceiling".to_string());
                }
            }
        }

        let section_key = if section == "capabilities.filesystem" {
            format!("capabilities.filesystem[{fs_index}].{key}")
        } else {
            format!("{section}.{key}")
        };
        match section.as_str() {
            "plugin" => {
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate key '{key}' in [plugin]"));
                }
                match key.as_str() {
                    "id" => id = Some(parse_string(&value)?),
                    "name" => name = Some(parse_string(&value)?),
                    "version" => version = Some(parse_string(&value)?),
                    "description" => description = parse_string(&value)?,
                    "license" => license = Some(parse_string(&value)?),
                    other => return Err(format!("unsupported [plugin] key '{other}'")),
                }
            }
            "compat" => {
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate key '{key}' in [compat]"));
                }
                match key.as_str() {
                    "bitty" => compat_bitty = Some(parse_string(&value)?),
                    "plugin-api" => compat_api = Some(parse_string(&value)?),
                    other => return Err(format!("unsupported [compat] key '{other}'")),
                }
            }
            "capabilities" => {
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate capability '{key}'"));
                }
                match parse_bool(&value) {
                    Some(true) => {
                        let id = CapabilityId::parse(&key)
                            .map_err(|e| format!("invalid capability '{key}': {e}"))?;
                        capabilities.insert(id);
                    }
                    Some(false) => {}
                    None => {
                        return Err(format!(
                            "capability '{key}' must be a boolean in this manifest subset"
                        ));
                    }
                }
            }
            "capabilities.filesystem" => {
                if !fs_entry_active {
                    return Err("manifest key outside a supported section".to_string());
                }
                if !seen_keys.insert(section_key) {
                    return Err(format!(
                        "duplicate key '{key}' in [[capabilities.filesystem]]"
                    ));
                }
                match key.as_str() {
                    "access" => {
                        fs_access = Some(parse_string(&value)?);
                    }
                    "paths" => {
                        fs_paths = Some(parse_array(&value)?);
                    }
                    other => {
                        return Err(format!(
                            "unsupported [[capabilities.filesystem]] key '{other}'"
                        ));
                    }
                }
            }
            "lazy" => {
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate [lazy] key '{key}'"));
                }
                match key.as_str() {
                    "commands" => lazy_commands = parse_array(&value)?,
                    "events" => lazy_events = parse_array(&value)?,
                    "claims" => lazy_claims = parse_array(&value)?,
                    other => return Err(format!("unsupported [lazy] key '{other}'")),
                }
            }
            s if s.starts_with("tools.") => {
                if !seen_keys.insert(section_key.clone()) {
                    return Err(format!("duplicate key '{key}' in [{s}]"));
                }
                match key.as_str() {
                    "required" => match parse_bool(&value) {
                        Some(b) => {
                            tools_git_required = Some(b);
                        }
                        None => {
                            return Err(format!(
                                "expected a boolean for '[{s}] required', found '{value}'"
                            ));
                        }
                    },
                    "version" => {
                        tools_git_version = Some(parse_string(&value)?);
                    }
                    other => return Err(format!("unsupported [{s}] key '{other}'")),
                }
            }
            _ => return Err("manifest key outside a supported section".to_string()),
        }
    }

    flush_filesystem_entry(
        &mut fs_access,
        &mut fs_paths,
        &mut filesystem,
        &mut fs_entry_active,
    )?;

    let mut tools = Vec::new();
    if tools_git_seen {
        let required = tools_git_required
            .ok_or("[tools.git] entry is missing 'required = true|false'".to_string())?;
        let version_req = tools_git_version
            .ok_or("[tools.git] entry is missing 'version = \"...\"'".to_string())?;
        tools.push(ToolDeclaration {
            tool: "git".to_string(),
            required,
            version_req,
        });
    }

    let identity = PluginIdentity {
        id: bitty_plugin_host::manifest::PluginId::new(&id.ok_or("manifest missing plugin.id")?)
            .map_err(|e| format!("invalid plugin.id: {e}"))?,
        name: name.ok_or("manifest missing plugin.name")?,
        version: version.ok_or("manifest missing plugin.version")?,
        description,
        license,
    };
    let mut commands = Vec::new();
    for command in lazy_commands {
        commands.push(
            QualifiedName::new(&command)
                .map_err(|e| format!("invalid lazy.commands entry '{command}': {e}"))?,
        );
    }
    let manifest = PluginManifest {
        identity,
        compat: Compat {
            bitty: compat_bitty,
            plugin_api: compat_api,
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: CapabilityRequests {
            ids: capabilities,
            filesystem,
        },
        tools,
        lazy: LazyTriggers {
            commands,
            events: lazy_events,
            claims: lazy_claims,
        },
        raw_bytes_len: bytes.len(),
    };
    manifest
        .validate()
        .map_err(|e| format!("manifest validation: {e}"))?;
    Ok(manifest)
}

/// Parse a TOML key, stripping surrounding quotes for parameterized forms.
///
/// Bare keys (`panel.provider`, `process.spawn:git` unquoted) pass through.
/// Double-quoted keys (`"process.spawn:git"`) are unescaped via [`parse_string`].
/// Single-quoted literal keys (`'process.spawn:git'`) are stripped without
/// escapes. Malformed quoting fails closed.
fn parse_key(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("expected a key".to_string());
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        return parse_string(raw);
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        let inner = &raw[1..raw.len() - 1];
        if inner.contains('\n') {
            return Err(format!("malformed key '{raw}'"));
        }
        return Ok(inner.to_string());
    }
    if raw.contains('"') || raw.contains('\'') {
        return Err(format!("malformed key '{raw}'"));
    }
    Ok(raw.to_string())
}

fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if *byte == b'"' {
            in_string = true;
        } else if *byte == b'#' {
            return &line[..index];
        }
    }
    line
}

fn balanced_brackets(value: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    for ch in value.chars() {
        match ch {
            '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth <= 0
}

fn parse_string(value: &str) -> Result<String, String> {
    let value = value.trim();
    if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
        return Err(format!("expected a quoted string, found '{value}'"));
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => return Err(format!("unsupported escape '\\{other}'")),
                None => return Err("truncated escape".to_string()),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_array(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    if !(value.starts_with('[') && value.ends_with(']')) {
        return Err(format!("expected an array, found '{value}'"));
    }
    let inner = &value[1..value.len() - 1];
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if in_string {
            if escaped {
                current.push(ch);
                escaped = false;
            } else if ch == '\\' {
                current.push(ch);
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                items.push(current.clone());
                current.clear();
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current.clear();
            }
            ',' | ' ' | '\n' | '\t' | '\r' => {}
            other => return Err(format!("unexpected character '{other}' in string array")),
        }
    }
    if in_string {
        return Err("unterminated string in array".to_string());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_body(id: &str) -> String {
        format!("[plugin]\nid = \"{id}\"\nname = \"T\"\nversion = \"0.1.0\"\ndescription = \"d\"\n")
    }

    #[test]
    fn quoted_capability_keys_are_accepted() {
        for key in [
            "\"process.spawn:git\"",
            "'process.spawn:git'",
            "process.spawn:git",
        ] {
            let body = format!(
                "{}\n[capabilities]\n{key} = true\npanel.provider = true\n[tools.git]\nrequired = true\nversion = \">=2.30\"\n",
                minimal_body("xuepoo.quoted")
            );
            // `process.spawn:git` requires `[tools.git]` and vice versa; both
            // are present here, so every quoting shape must parse.
            let manifest = parse_manifest(body.as_bytes()).expect("quoted key shape must parse");
            assert!(
                manifest
                    .capabilities
                    .ids
                    .iter()
                    .any(|c| c.as_str() == "process.spawn:git")
            );
        }
    }

    #[test]
    fn git_panel_fixture_parses_with_filesystem_and_tools() {
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n\"process.spawn:git\" = true\n[[capabilities.filesystem]]\naccess = \"read\"\npaths = [\"~/projects/**\"]\n[tools.git]\nrequired = true\nversion = \">=2.30\"\n",
            minimal_body("xuepoo.gitpanel")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("git-panel shape must parse");
        assert_eq!(manifest.capabilities.filesystem.len(), 1);
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.tools[0].tool, "git");
    }

    #[test]
    fn unknown_tools_section_fails_closed() {
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[tools.rg]\nrequired = true\nversion = \">=13\"\n",
            minimal_body("xuepoo.unknown")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn tools_git_rejects_unknown_keys_and_bad_types() {
        // Extra `args` key is verb smuggling via the manifest (fail closed).
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:git\" = true\n[tools.git]\nrequired = true\nversion = \">=2.30\"\nargs = [\"--exec=evil\"]\n",
            minimal_body("xuepoo.smuggle")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // `required = "yes"` (string, not bool) fails closed.
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:git\" = true\n[tools.git]\nrequired = \"yes\"\nversion = \">=2.30\"\n",
            minimal_body("xuepoo.badreq")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // `version = 42` (numeric, not string) fails closed.
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:git\" = true\n[tools.git]\nrequired = true\nversion = 42\n",
            minimal_body("xuepoo.badver")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Missing `version` fails closed.
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:git\" = true\n[tools.git]\nrequired = true\n",
            minimal_body("xuepoo.nover")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn spawn_without_tool_and_tool_without_spawn_fail_closed() {
        // Capability without declaration.
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:git\" = true\n",
            minimal_body("xuepoo.ntool")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Declaration without capability.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[tools.git]\nrequired = true\nversion = \">=2.30\"\n",
            minimal_body("xuepoo.ncap")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn filesystem_single_bracket_and_bad_access_fail_closed() {
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[capabilities.filesystem]\naccess = \"read\"\n",
            minimal_body("xuepoo.single")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[[capabilities.filesystem]]\naccess = \"evil\"\npaths = [\"~/projects/**\"]\n",
            minimal_body("xuepoo.badaccess")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn filesystem_bool_keys_fail_closed() {
        // `fs.read` / `fs.write` as bool keys must fail (use the table).
        let body = format!(
            "{}\n[capabilities]\n\"fs.read:~/projects/**\" = true\n",
            minimal_body("xuepoo.fsbool")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn bad_param_and_bare_spawn_fail_closed() {
        // Evil param with separator smuggling.
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:evil;rm\" = true\n[tools.git]\nrequired = true\nversion = \">=2.30\"\n",
            minimal_body("xuepoo.badparam")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Double-colon param.
        let body = format!(
            "{}\n[capabilities]\n\"process.spawn:a:b\" = true\n[tools.git]\nrequired = true\nversion = \">=2.30\"\n",
            minimal_body("xuepoo.doublecolon")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Bare `process.spawn` without param.
        let body = format!(
            "{}\n[capabilities]\nprocess.spawn = true\n[tools.git]\nrequired = true\nversion = \">=2.30\"\n",
            minimal_body("xuepoo.barespawn")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }
}
