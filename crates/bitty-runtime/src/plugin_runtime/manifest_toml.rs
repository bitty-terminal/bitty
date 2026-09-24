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
//! `[limits]`, `[dependencies]`, `[services.provided]`, `[services.required]`,
//! `[tools.git]` (accepted Layer-2 v1, CTX-0425),
//! `[[capabilities.filesystem]]` and `[[network.egress]]` (array-of-tables).
//! Strings, booleans, arrays of strings, and (in `[limits]` only) bare
//! non-negative integers are supported. Inline-table values are accepted for
//! the declared shapes only: `[dependencies]` `{ version, prerelease }`,
//! `[services.provided]` `{ version, args_schema, result_schema }`,
//! `[services.required]` `{ version }`, and `[lazy].commands` elements
//! `{ id, args_schema, result_schema }`. Schemas are bounded JSON documents
//! (16 KiB per schema, depth at most 16, explicit `additionalProperties`).
//! Unknown sections, sub-tables, other table keys,
//! other numeric values, and duplicate keys fail closed.
//!
//! The string forms stay accepted everywhere (`"owner.name" = ">=2.0"`,
//! `"iface" = "1.0.0"`, `commands = ["owner:cmd"]`) and carry no schemas and
//! no prerelease opt-in.

use std::collections::BTreeSet;

use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::manifest::{
    ACCEPTED_TOOLS, CapabilityRequests, Compat, FilesystemRequest, FsAccess, LazyCommand,
    LazyTriggers, NetworkEgress, PluginDependency, PluginIdentity, PluginLimits, PluginManifest,
    ProvidedService, QualifiedName, ToolDeclaration,
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
    let mut lazy_commands: Vec<LazyCommand> = Vec::new();
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

    // `[[network.egress]]` array entries.
    let mut network: Vec<NetworkEgress> = Vec::new();
    let mut egress_host: Option<String> = None;
    let mut egress_ports: Option<Vec<u16>> = None;
    let mut egress_index: usize = 0;
    let mut egress_entry_active = false;

    // `[limits]` budgets (all keys optional, bare integers only).
    let mut limits = PluginLimits::default();

    // `[dependencies]` entries: `"owner.name" = ">=2.0"` or
    // `"owner.name" = { version = ">=2.0", prerelease = true }`.
    let mut dependencies: Vec<PluginDependency> = Vec::new();
    // `[services.provided]` entries: `"iface" = "1.0.0"` or
    // `"iface" = { version = "1.0.0", args_schema = "{...}", result_schema = "{...}" }`.
    let mut provided_services: Vec<ProvidedService> = Vec::new();
    // `[services.required]` entries: `"iface" = ">=1.2"` or
    // `"iface" = { version = ">=1.2" }` (schemas are provider-side only).
    let mut required_services: Vec<(String, String)> = Vec::new();

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

    let flush_egress_entry = |egress_host: &mut Option<String>,
                              egress_ports: &mut Option<Vec<u16>>,
                              network: &mut Vec<NetworkEgress>,
                              egress_entry_active: &mut bool|
     -> Result<(), String> {
        if !*egress_entry_active {
            return Ok(());
        }
        let host = egress_host
            .take()
            .ok_or("[[network.egress]] entry is missing 'host = \"...\"'".to_string())?;
        let ports = egress_ports
            .take()
            .ok_or("[[network.egress]] entry is missing 'ports = [...]'".to_string())?;
        network.push(NetworkEgress { host, ports });
        *egress_entry_active = false;
        Ok(())
    };

    let mut lines = text.lines().enumerate().peekable();
    while let Some((line_number, raw_line)) = lines.next() {
        let line = strip_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // Array-of-tables `[[...]]` (only `[[capabilities.filesystem]]`
            // and `[[network.egress]]`).
            if line.starts_with("[[") && line.ends_with("]]") {
                let inner = line[2..line.len() - 2].trim().to_string();
                if inner != "capabilities.filesystem" && inner != "network.egress" {
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
                flush_egress_entry(
                    &mut egress_host,
                    &mut egress_ports,
                    &mut network,
                    &mut egress_entry_active,
                )?;
                if inner == "capabilities.filesystem" {
                    fs_entry_active = true;
                    // Index for dedupe is the count of already-flushed entries.
                    fs_index = filesystem.len();
                } else {
                    egress_entry_active = true;
                    egress_index = network.len();
                }
                section = inner;
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
            // Reject single-bracket network egress (must be `[[...]]` array).
            if inner == "network.egress" || inner == "network" {
                return Err(format!(
                    "unsupported manifest section '[{inner}]' at line {} (expected '[[network.egress]]')",
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
                // Flush any open filesystem/network entry before switching sections.
                flush_filesystem_entry(
                    &mut fs_access,
                    &mut fs_paths,
                    &mut filesystem,
                    &mut fs_entry_active,
                )?;
                flush_egress_entry(
                    &mut egress_host,
                    &mut egress_ports,
                    &mut network,
                    &mut egress_entry_active,
                )?;
                tools_git_seen = true;
                section = inner;
                continue;
            }
            match inner.as_str() {
                "plugin" | "compat" | "capabilities" | "lazy" | "limits" | "dependencies"
                | "services.provided" | "services.required" => {
                    flush_filesystem_entry(
                        &mut fs_access,
                        &mut fs_paths,
                        &mut filesystem,
                        &mut fs_entry_active,
                    )?;
                    flush_egress_entry(
                        &mut egress_host,
                        &mut egress_ports,
                        &mut network,
                        &mut egress_entry_active,
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

        // Multi-line arrays and inline tables: keep consuming until all
        // brackets and braces close.
        if !balanced_delims(&value) {
            for (next_number, next_raw) in lines.by_ref() {
                let next = strip_comment(next_raw).trim().to_string();
                value.push(' ');
                value.push_str(&next);
                if balanced_delims(&value) {
                    break;
                }
                if next_number > line_number + 4096 {
                    return Err("manifest value exceeded the line ceiling".to_string());
                }
            }
        }

        let section_key = if section == "capabilities.filesystem" {
            format!("capabilities.filesystem[{fs_index}].{key}")
        } else if section == "network.egress" {
            format!("network.egress[{egress_index}].{key}")
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
                    "commands" => {
                        lazy_commands = parse_commands_array(&value)
                            .map_err(|e| format!("invalid [lazy] commands: {e}"))?;
                    }
                    "events" => lazy_events = parse_array(&value)?,
                    "claims" => lazy_claims = parse_array(&value)?,
                    other => return Err(format!("unsupported [lazy] key '{other}'")),
                }
            }
            "network.egress" => {
                if !egress_entry_active {
                    return Err("manifest key outside a supported section".to_string());
                }
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate key '{key}' in [[network.egress]]"));
                }
                match key.as_str() {
                    "host" => {
                        egress_host = Some(parse_string(&value)?);
                    }
                    "ports" => {
                        egress_ports = Some(
                            parse_port_array(&value)
                                .map_err(|e| format!("invalid [[network.egress]] ports: {e}"))?,
                        );
                    }
                    other => {
                        return Err(format!("unsupported [[network.egress]] key '{other}'"));
                    }
                }
            }
            "limits" => {
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate [limits] key '{key}'"));
                }
                let slot = match key.as_str() {
                    "max_commands" => &mut limits.max_commands,
                    "max_event_types" => &mut limits.max_event_types,
                    "max_tools" => &mut limits.max_tools,
                    "max_pattern_text_bytes" => &mut limits.max_pattern_text_bytes,
                    "max_dependencies" => &mut limits.max_dependencies,
                    "max_provided_services" => &mut limits.max_provided_services,
                    "max_required_services" => &mut limits.max_required_services,
                    "max_fs_patterns_per_kind" => &mut limits.max_fs_patterns_per_kind,
                    "max_network_egress" => &mut limits.max_network_egress,
                    "max_network_ports_per_host" => &mut limits.max_network_ports_per_host,
                    other => return Err(format!("unsupported [limits] key '{other}'")),
                };
                *slot = Some(
                    parse_limit_uint(&value, &key)
                        .map_err(|e| format!("invalid [limits] key '{key}': {e}"))?,
                );
            }
            "dependencies" | "services.provided" | "services.required" => {
                if !seen_keys.insert(section_key) {
                    return Err(format!("duplicate key '{key}' in [{section}]"));
                }
                match section.as_str() {
                    "dependencies" => {
                        dependencies.push(parse_dependency_entry(&key, &value)?);
                    }
                    "services.provided" => {
                        provided_services.push(parse_provided_entry(&key, &value)?);
                    }
                    _ => {
                        let req = parse_required_entry(&key, &value)?;
                        required_services.push((key, req));
                    }
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
    flush_egress_entry(
        &mut egress_host,
        &mut egress_ports,
        &mut network,
        &mut egress_entry_active,
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
    let mut commands: Vec<LazyCommand> = Vec::new();
    for command in lazy_commands {
        if commands.iter().any(|seen| seen.id == command.id) {
            return Err(format!(
                "duplicate [lazy] commands entry '{}'",
                command.id.as_str()
            ));
        }
        commands.push(command);
    }
    let mut parsed_dependencies: Vec<PluginDependency> = Vec::new();
    for dep in dependencies {
        if parsed_dependencies.iter().any(|seen| seen.id == dep.id) {
            return Err(format!("duplicate [dependencies] id '{}'", dep.id.as_str()));
        }
        parsed_dependencies.push(dep);
    }
    let manifest = PluginManifest {
        identity,
        compat: Compat {
            bitty: compat_bitty,
            plugin_api: compat_api,
        },
        dependencies: parsed_dependencies,
        provided_services,
        required_services,
        capabilities: CapabilityRequests {
            ids: capabilities,
            filesystem,
        },
        tools,
        network,
        limits,
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

fn balanced_delims(value: &str) -> bool {
    let mut brackets = 0i32;
    let mut braces = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in value.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => brackets += 1,
            ']' => {
                brackets -= 1;
                if brackets < 0 {
                    return true;
                }
            }
            '{' => braces += 1,
            '}' => {
                braces -= 1;
                if braces < 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    !in_string && brackets <= 0 && braces <= 0
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

/// Parse a `[limits]` budget: bare ASCII digits only (fail-closed).
///
/// Quoted strings, signs, hex, floats, and empty values are rejected: limits
/// are integers in real TOML and the subset keeps that shape. Overflow fails
/// closed. Ceiling/zero checks belong to [`PluginLimits::validate`].
fn parse_limit_uint(value: &str, key: &str) -> Result<usize, String> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "expected a bare non-negative integer for '{key}', found '{value}'"
        ));
    }
    // Bound the digit run before parsing (fail fast on absurd input).
    if value.len() > 10 {
        return Err(format!("integer for '{key}' is too large"));
    }
    value
        .parse::<usize>()
        .map_err(|_| format!("integer for '{key}' is too large"))
}

/// Parse a `[[network.egress]]` port list: `[443, "993"]`.
///
/// Items may be bare ASCII-digit runs (real TOML integers) or quoted decimal
/// strings; both must fit in `u16`. Signs, hex, floats, empty items, and
/// non-numeric junk fail closed. At least one item is required (an empty
/// array fails here; port 0 fails later in [`NetworkEgress::validate`]).
fn parse_port_array(value: &str) -> Result<Vec<u16>, String> {
    let value = value.trim();
    if !(value.starts_with('[') && value.ends_with(']')) {
        return Err(format!("expected an array, found '{value}'"));
    }
    let inner = &value[1..value.len() - 1];
    let mut ports = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let digits = if item.starts_with('"') && item.ends_with('"') && item.len() >= 2 {
            parse_string(item)?
        } else {
            item.to_string()
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("expected a decimal port, found '{item}'"));
        }
        if digits.len() > 5 {
            return Err(format!("port '{item}' is too large"));
        }
        let port: u16 = digits
            .parse()
            .map_err(|_| format!("port '{item}' is too large"))?;
        ports.push(port);
    }
    if ports.is_empty() {
        return Err("ports array must list at least one port".to_string());
    }
    Ok(ports)
}

/// Parse one `[dependencies]` value: string form or `{ version, prerelease }`.
fn parse_dependency_entry(key: &str, value: &str) -> Result<PluginDependency, String> {
    let id = bitty_plugin_host::manifest::PluginId::new(key)
        .map_err(|e| format!("invalid [dependencies] id '{key}': {e}"))?;
    if value.trim_start().starts_with('{') {
        let fields = parse_inline_table(value)
            .map_err(|e| format!("invalid '[dependencies] {key}': {e}"))?;
        let version = inline_required_string(&fields, "version", "dependencies", key)?;
        let prerelease = inline_optional_bool(&fields, "prerelease", "dependencies", key)?;
        reject_inline_keys(&fields, &["version", "prerelease"], "dependencies", key)?;
        return Ok(PluginDependency {
            id,
            req: version,
            prerelease,
        });
    }
    let req = parse_string(value).map_err(|e| format!("invalid '[dependencies] {key}': {e}"))?;
    Ok(PluginDependency {
        id,
        req,
        prerelease: false,
    })
}

/// Parse one `[services.provided]` value: string form or
/// `{ version, args_schema, result_schema }`.
fn parse_provided_entry(key: &str, value: &str) -> Result<ProvidedService, String> {
    if value.trim_start().starts_with('{') {
        let fields = parse_inline_table(value)
            .map_err(|e| format!("invalid '[services.provided] {key}': {e}"))?;
        let version = inline_required_string(&fields, "version", "services.provided", key)?;
        let args_schema = inline_optional_string(&fields, "args_schema", "services.provided", key)?;
        let result_schema =
            inline_optional_string(&fields, "result_schema", "services.provided", key)?;
        reject_inline_keys(
            &fields,
            &["version", "args_schema", "result_schema"],
            "services.provided",
            key,
        )?;
        return Ok(ProvidedService {
            iface: key.to_string(),
            version,
            args_schema,
            result_schema,
        });
    }
    let version =
        parse_string(value).map_err(|e| format!("invalid '[services.provided] {key}': {e}"))?;
    Ok(ProvidedService {
        iface: key.to_string(),
        version,
        args_schema: None,
        result_schema: None,
    })
}

/// Parse one `[services.required]` value: string form or `{ version }`.
///
/// Schemas are provider-side only: `args_schema` / `result_schema` keys fail
/// closed here so a consumer can never smuggle a provider shape.
fn parse_required_entry(key: &str, value: &str) -> Result<String, String> {
    if value.trim_start().starts_with('{') {
        let fields = parse_inline_table(value)
            .map_err(|e| format!("invalid '[services.required] {key}': {e}"))?;
        let version = inline_required_string(&fields, "version", "services.required", key)?;
        reject_inline_keys(&fields, &["version"], "services.required", key)?;
        return Ok(version);
    }
    parse_string(value).map_err(|e| format!("invalid '[services.required] {key}': {e}"))
}

/// Parse the `[lazy] commands` array: string elements or
/// `{ id, args_schema, result_schema }` inline tables.
fn parse_commands_array(value: &str) -> Result<Vec<LazyCommand>, String> {
    let value = value.trim();
    if !(value.starts_with('[') && value.ends_with(']')) {
        return Err(format!("expected an array, found '{value}'"));
    }
    let inner = &value[1..value.len() - 1];
    let mut commands = Vec::new();
    for item in split_top_level(inner, ',')? {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item.starts_with('{') {
            let fields =
                parse_inline_table(item).map_err(|e| format!("invalid commands entry: {e}"))?;
            let id_raw = inline_required_string(&fields, "id", "lazy.commands", "<entry>")?;
            let id = QualifiedName::new(&id_raw)
                .map_err(|e| format!("invalid lazy.commands entry '{id_raw}': {e}"))?;
            let args_schema =
                inline_optional_string(&fields, "args_schema", "lazy.commands", &id_raw)?;
            let result_schema =
                inline_optional_string(&fields, "result_schema", "lazy.commands", &id_raw)?;
            reject_inline_keys(
                &fields,
                &["id", "args_schema", "result_schema"],
                "lazy.commands",
                &id_raw,
            )?;
            commands.push(LazyCommand {
                id,
                args_schema,
                result_schema,
            });
        } else {
            let id_raw = parse_string(item)
                .map_err(|e| format!("invalid lazy.commands entry '{item}': {e}"))?;
            let id = QualifiedName::new(&id_raw)
                .map_err(|e| format!("invalid lazy.commands entry '{id_raw}': {e}"))?;
            commands.push(LazyCommand {
                id,
                args_schema: None,
                result_schema: None,
            });
        }
    }
    Ok(commands)
}

/// Parse `{ key = value, ... }` into raw (key, value-text) pairs.
///
/// Keys are bare or quoted (via [`parse_key`]); values stay raw — quoted
/// strings (unescaped by the caller) or bare literals (`true`/`false`).
/// Top-level commas split pairs; commas inside strings are respected.
/// Nested-table values fail closed at interpretation (no declared shape nests).
fn parse_inline_table(value: &str) -> Result<Vec<(String, String)>, String> {
    let value = value.trim();
    if !(value.starts_with('{') && value.ends_with('}')) {
        return Err(format!("expected an inline table, found '{value}'"));
    }
    let inner = &value[1..value.len() - 1];
    let mut pairs = Vec::new();
    for part in split_top_level(inner, ',')? {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = part
            .split_once('=')
            .ok_or_else(|| format!("expected 'key = value' in inline table, found '{part}'"))?;
        let key = parse_key(raw_key.trim())?;
        if pairs.iter().any(|(seen, _)| seen == &key) {
            return Err(format!("duplicate key '{key}' in inline table"));
        }
        pairs.push((key, raw_value.trim().to_string()));
    }
    Ok(pairs)
}

/// Split on a top-level separator, respecting double-quoted strings (with
/// `\` escapes) and `{...}`/`[...]` nesting. Anything unbalanced fails closed.
fn split_top_level(inner: &str, sep: char) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0usize;
    for ch in inner.chars() {
        if in_string {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current.push(ch);
            }
            '{' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '}' | ']' => {
                if depth == 0 {
                    return Err(format!("unbalanced '{ch}' in manifest value"));
                }
                depth -= 1;
                current.push(ch);
            }
            _ if ch == sep && depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if in_string {
        return Err("unterminated string in manifest value".to_string());
    }
    if depth != 0 {
        return Err("unbalanced brackets in manifest value".to_string());
    }
    parts.push(current);
    Ok(parts)
}

/// Required string field of an inline table.
fn inline_required_string(
    fields: &[(String, String)],
    name: &str,
    section: &str,
    entry: &str,
) -> Result<String, String> {
    let raw = fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| format!("'[{section}] {entry}' is missing '{name} = \"...\"'"))?;
    if raw.trim_start().starts_with('{') {
        return Err(format!(
            "'[{section}] {entry}' field '{name}' must be a string (nested tables are not supported)"
        ));
    }
    parse_string(raw).map_err(|e| format!("'[{section}] {entry}' field '{name}': {e}"))
}

/// Optional string field of an inline table.
fn inline_optional_string(
    fields: &[(String, String)],
    name: &str,
    section: &str,
    entry: &str,
) -> Result<Option<String>, String> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| {
            if value.trim_start().starts_with('{') {
                return Err(format!(
                    "'[{section}] {entry}' field '{name}' must be a string (nested tables are not supported)"
                ));
            }
            parse_string(value).map_err(|e| format!("'[{section}] {entry}' field '{name}': {e}"))
        })
        .transpose()
}

/// Optional boolean field of an inline table (absent means `false`).
fn inline_optional_bool(
    fields: &[(String, String)],
    name: &str,
    section: &str,
    entry: &str,
) -> Result<bool, String> {
    match fields.iter().find(|(key, _)| key == name) {
        None => Ok(false),
        Some((_, value)) => match value.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(format!(
                "'[{section}] {entry}' field '{name}' must be 'true' or 'false', found '{other}'"
            )),
        },
    }
}

/// Reject unknown inline-table keys (fail-closed against smuggled fields).
fn reject_inline_keys(
    fields: &[(String, String)],
    allowed: &[&str],
    section: &str,
    entry: &str,
) -> Result<(), String> {
    for (key, _) in fields {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported '[{section}] {entry}' field '{key}'"));
        }
    }
    Ok(())
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
    fn network_egress_parses_and_pairs_with_capability() {
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n\"network.connect:example.com:443\" = true\n[[network.egress]]\nhost = \"example.com\"\nports = [443]\n",
            minimal_body("xuepoo.nettoml")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("network shape must parse");
        assert_eq!(manifest.network.len(), 1);
        assert_eq!(manifest.network[0].host, "example.com");
        assert_eq!(manifest.network[0].ports, vec![443]);
    }

    #[test]
    fn network_egress_requires_pairing_both_directions() {
        // Capability without an egress entry fails closed.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n\"network.connect:example.com:443\" = true\n",
            minimal_body("xuepoo.netnocov")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Egress entry without a capability fails closed.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[[network.egress]]\nhost = \"example.com\"\nports = [443]\n",
            minimal_body("xuepoo.netnocap")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Undeclared port fails closed.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n\"network.connect:example.com:993\" = true\n[[network.egress]]\nhost = \"example.com\"\nports = [443]\n",
            minimal_body("xuepoo.netport")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn network_egress_rejects_bad_sections_keys_and_ports() {
        // Single-bracket `[network]` / `[network.egress]` fail closed.
        for section in ["[network]", "[network.egress]"] {
            let body = format!(
                "{}\n[capabilities]\npanel.provider = true\n{section}\nhost = \"example.com\"\n",
                minimal_body("xuepoo.netsingle")
            );
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "{section} must fail closed"
            );
        }

        // Unknown keys, missing keys, and bad port shapes fail closed.
        for fragment in [
            "host = \"example.com\"\nports = [443]\nport = 443\n",
            "ports = [443]\n",
            "host = \"example.com\"\n",
            "host = \"example.com\"\nports = []\n",
            "host = \"example.com\"\nports = [-1]\n",
            "host = \"example.com\"\nports = [99999]\n",
            "host = \"*.example.com\"\nports = [443]\n",
            "host = \"Example.COM\"\nports = [443]\n",
            "host = \"example.com\"\nports = 443\n",
        ] {
            let body = format!(
                "{}\n[capabilities]\npanel.provider = true\n\"network.connect:example.com:443\" = true\n[[network.egress]]\n{fragment}",
                minimal_body("xuepoo.netbad")
            );
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "fragment must fail closed: {fragment:?}"
            );
        }

        // Duplicate keys within one entry fail closed.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n\"network.connect:example.com:443\" = true\n[[network.egress]]\nhost = \"example.com\"\nhost = \"example.com\"\nports = [443]\n",
            minimal_body("xuepoo.netdup")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }

    #[test]
    fn limits_parse_and_enforce() {
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[limits]\nmax_commands = 4\nmax_tools = 2\n",
            minimal_body("xuepoo.limtoml")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("limits shape must parse");
        assert_eq!(manifest.limits.max_commands, Some(4));
        assert_eq!(manifest.limits.max_tools, Some(2));
        assert_eq!(manifest.limits.max_event_types, None);
    }

    #[test]
    fn limits_reject_bad_keys_values_and_types() {
        for fragment in [
            // Unknown key.
            "max_everything = 4\n",
            // Above the host ceiling (escalation attempt).
            "max_commands = 129\n",
            // Zero is meaningless.
            "max_commands = 0\n",
            // Quoted, signed, float, and empty values fail closed.
            "max_commands = \"4\"\n",
            "max_commands = -1\n",
            "max_commands = 4.0\n",
            "max_commands = \n",
        ] {
            let body = format!(
                "{}\n[capabilities]\npanel.provider = true\n[limits]\n{fragment}",
                minimal_body("xuepoo.limbad")
            );
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "fragment must fail closed: {fragment:?}"
            );
        }

        // Duplicate keys fail closed.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[limits]\nmax_commands = 4\nmax_commands = 4\n",
            minimal_body("xuepoo.limdup")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Declared budget exceeded by the manifest's own contents fails.
        let body = format!(
            "{}\n[capabilities]\npanel.provider = true\n[limits]\nmax_commands = 0\n",
            minimal_body("xuepoo.limself")
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

    #[test]
    fn dependencies_and_services_string_forms_parse() {
        let body = format!(
            "{}\n[dependencies]\n\"xuepoo.gitcore\" = \">=2.0\"\n[services.provided]\n\"markdown.render\" = \"1.0.0\"\n[services.required]\n\"git.status\" = \">=1.2\"\n",
            minimal_body("xuepoo.depsvc")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("deps/services shape must parse");
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].id.as_str(), "xuepoo.gitcore");
        assert_eq!(manifest.dependencies[0].req, ">=2.0");
        assert!(!manifest.dependencies[0].prerelease);
        assert_eq!(manifest.provided_services.len(), 1);
        assert_eq!(manifest.provided_services[0].iface, "markdown.render");
        assert_eq!(manifest.provided_services[0].version, "1.0.0");
        assert!(!manifest.provided_services[0].has_schemas());
        assert_eq!(
            manifest.required_services,
            vec![("git.status".to_string(), ">=1.2".to_string())]
        );
    }

    #[test]
    fn dependency_table_form_parses_with_prerelease_opt_in() {
        let body = format!(
            "{}\n[dependencies]\n\"xuepoo.gitcore\" = {{ version = \">=2.0\", prerelease = true }}\n\"xuepoo.stable\" = {{ version = \"^1.0\" }}\n",
            minimal_body("xuepoo.deptable")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("table form must parse");
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.dependencies[0].id.as_str(), "xuepoo.gitcore");
        assert_eq!(manifest.dependencies[0].req, ">=2.0");
        assert!(manifest.dependencies[0].prerelease);
        assert!(!manifest.dependencies[1].prerelease);
        // The prerelease bit plumbs through to the resolver edge.
        let edge = manifest.dependencies[0]
            .as_package_edge()
            .expect("plugin id converts to package id");
        assert!(edge.prerelease);
        assert_eq!(edge.version_req, ">=2.0");
    }

    #[test]
    fn provided_table_form_parses_with_schemas() {
        let args = "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"],\"additionalProperties\":false}";
        let result = "{\"type\":\"object\",\"additionalProperties\":false}";
        let body = format!(
            "{}\n[services.provided]\n\"markdown.render\" = {{ version = \"1.0.0\", args_schema = \"{args}\", result_schema = \"{result}\" }}\n",
            minimal_body("xuepoo.svctable")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("table form must parse");
        assert_eq!(manifest.provided_services.len(), 1);
        let svc = &manifest.provided_services[0];
        assert_eq!(svc.iface, "markdown.render");
        assert_eq!(svc.version, "1.0.0");
        assert!(svc.has_schemas());
        assert_eq!(svc.args_schema.as_deref(), Some(args));
    }

    #[test]
    fn required_table_form_accepts_version_only() {
        let body = format!(
            "{}\n[services.required]\n\"git.status\" = {{ version = \">=1.2\" }}\n",
            minimal_body("xuepoo.reqtable")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("table form must parse");
        assert_eq!(
            manifest.required_services,
            vec![("git.status".to_string(), ">=1.2".to_string())]
        );
    }

    #[test]
    fn lazy_commands_table_form_parses_with_schemas() {
        let args = "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"],\"additionalProperties\":false}";
        let body = format!(
            "{}\n[lazy]\ncommands = [\"xuepoo.plain:run\", {{ id = \"xuepoo.typed:open\", args_schema = \"{args}\" }}]\n",
            minimal_body("xuepoo.cmdtable")
        );
        let manifest = parse_manifest(body.as_bytes()).expect("table form must parse");
        assert_eq!(manifest.lazy.commands.len(), 2);
        assert_eq!(manifest.lazy.commands[0].id.as_str(), "xuepoo.plain:run");
        assert!(manifest.lazy.commands[0].args_schema.is_none());
        assert_eq!(manifest.lazy.commands[1].id.as_str(), "xuepoo.typed:open");
        assert_eq!(manifest.lazy.commands[1].args_schema.as_deref(), Some(args));
    }

    #[test]
    fn dependencies_and_services_fail_closed() {
        // Bare `[services]` (without `.provided`/`.required`) fails closed.
        let body = format!(
            "{}\n[services]\n\"markdown.render\" = \"1.0.0\"\n",
            minimal_body("xuepoo.baresvc")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());

        // Malformed inline tables fail closed: missing version, bad types,
        // unknown keys, nested tables, duplicate keys.
        for fragment in [
            "[dependencies]\n\"xuepoo.gitcore\" = { prerelease = true }\n",
            "[dependencies]\n\"xuepoo.gitcore\" = { version = \">=2.0\", prerelease = \"yes\" }\n",
            "[dependencies]\n\"xuepoo.gitcore\" = { version = \">=2.0\", channel = \"nightly\" }\n",
            "[dependencies]\n\"xuepoo.gitcore\" = { version = { min = \">=2.0\" } }\n",
            "[dependencies]\n\"xuepoo.gitcore\" = { version = \">=2.0\", version = \"^1.0\" }\n",
            "[services.provided]\n\"markdown.render\" = { args_schema = \"{}\" }\n",
            "[services.provided]\n\"markdown.render\" = { version = \"1.0.0\", unknown = \"x\" }\n",
            "[services.required]\n\"git.status\" = { version = \">=1.2\", args_schema = \"{}\" }\n",
            "[services.required]\n\"git.status\" = { version = \">=1.2\", version = \"^1.0\" }\n",
        ] {
            let body = format!("{}\n{fragment}", minimal_body("xuepoo.svcbad"));
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "fragment must fail closed: {fragment:?}"
            );
        }

        // Hostile schemas fail closed: oversized, non-object, open object,
        // non-boolean additionalProperties, over-deep, malformed JSON.
        let big_schema = format!("\"{}\"}}", "x".repeat(16 * 1024));
        let deep_schema = format!("{}\"x\"{}", "{\"a\":".repeat(20), "}".repeat(20));
        for fragment in [
            format!(
                "[services.provided]\n\"markdown.render\" = {{ version = \"1.0.0\", args_schema = {big_schema} }}\n"
            ),
            "[services.provided]\n\"markdown.render\" = { version = \"1.0.0\", args_schema = \"[]\" }\n"
                .to_string(),
            "[services.provided]\n\"markdown.render\" = { version = \"1.0.0\", args_schema = \"{\\\"properties\\\":{}}\" }\n"
                .to_string(),
            "[services.provided]\n\"markdown.render\" = { version = \"1.0.0\", args_schema = \"{\\\"properties\\\":{},\\\"additionalProperties\\\":\\\"no\\\"}\" }\n"
                .to_string(),
            format!(
                "[services.provided]\n\"markdown.render\" = {{ version = \"1.0.0\", args_schema = \"{deep_schema}\" }}\n"
            ),
            "[services.provided]\n\"markdown.render\" = { version = \"1.0.0\", args_schema = \"{oops\" }\n"
                .to_string(),
        ] {
            let body = format!("{}\n{fragment}", minimal_body("xuepoo.svcbad2"));
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "fragment must fail closed: {fragment:?}"
            );
        }

        // Bad lazy command entries fail closed: missing id, unknown keys,
        // bare (unquoted) ids, duplicate commands across string/table forms.
        for fragment in [
            "[lazy]\ncommands = [{ args_schema = \"{}\" }]\n",
            "[lazy]\ncommands = [{ id = \"xuepoo.a:run\", help = \"run it\" }]\n",
            "[lazy]\ncommands = [xuepoo.a:run]\n",
            "[lazy]\ncommands = [\"xuepoo.a:run\", { id = \"xuepoo.a:run\" }]\n",
        ] {
            let body = format!("{}\n{fragment}", minimal_body("xuepoo.cmdbad"));
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "fragment must fail closed: {fragment:?}"
            );
        }

        // Invalid dependency id, bad version requirement, and bad semver fail.
        for fragment in [
            "[dependencies]\n\"BAD-ID\" = \">=2.0\"\n",
            "[dependencies]\n\"xuepoo.gitcore\" = \">=2.0; rm -rf\"\n",
            "[services.provided]\n\"markdown.render\" = \"not-a-version\"\n",
            "[services.provided]\n\"markdown.render\" = 42\n",
        ] {
            let body = format!("{}\n{fragment}", minimal_body("xuepoo.svcbad2"));
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "fragment must fail closed: {fragment:?}"
            );
        }

        // Duplicate keys within one section fail closed.
        let body = format!(
            "{}\n[dependencies]\n\"xuepoo.gitcore\" = \">=2.0\"\n\"xuepoo.gitcore\" = \">=2.0\"\n",
            minimal_body("xuepoo.svcdup")
        );
        assert!(parse_manifest(body.as_bytes()).is_err());
    }
}
