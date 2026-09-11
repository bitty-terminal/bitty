//! Bounded reader for the `bitty-plugin.toml` manifest subset.
//!
//! The full package-integrity pipeline (signatures, artifact verification, the
//! complete TOML schema) stays with the package manager; the runtime's Gap B
//! resolution re-verifies the parsed manifest's `manifest_hash` and the module
//! tree's `content_digest` against the stored record. This module is the
//! minimal, fail-closed reader used for both, and performs no I/O: the caller
//! supplies the already-bounded bytes.
//!
//! Supported sections: `[plugin]`, `[compat]`, `[capabilities]`, `[lazy]`.
//! Strings, booleans, and arrays of strings are supported. Unknown sections,
//! sub-tables, numeric values, and duplicate keys fail closed.

use std::collections::BTreeSet;

use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::manifest::{
    CapabilityRequests, Compat, LazyTriggers, PluginIdentity, PluginManifest, QualifiedName,
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

    let mut lines = text.lines().enumerate().peekable();
    while let Some((line_number, raw_line)) = lines.next() {
        let line = strip_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            match section.as_str() {
                "plugin" | "compat" | "capabilities" | "lazy" => {}
                other => {
                    return Err(format!(
                        "unsupported manifest section '[{other}]' at line {}",
                        line_number + 1
                    ));
                }
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("expected 'key = value' at line {}", line_number + 1))?;
        let key = key.trim().to_string();
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

        let dedupe = format!("{section}.{key}");
        let section_key = format!("{section}.{key}");
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
            _ => return Err("manifest key outside a supported section".to_string()),
        }
        let _ = dedupe;
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
            filesystem: Vec::new(),
        },
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
