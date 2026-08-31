//! Direct, non-shell URL launching.

use std::process::Command;

use crate::error::PlatformError;

/// Maximum URI length accepted by the platform boundary.
pub const URL_MAX_LEN: usize = 4096;

/// An URI which has passed the platform URL policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedUrl(String);

impl ValidatedUrl {
    /// Returns the original URI without normalization.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validates a URI before it can reach an OS URL handler.
pub fn validate_url(uri: &str) -> Result<ValidatedUrl, PlatformError> {
    let Some(end) = uri.find(':') else {
        return Err(PlatformError::InvalidUrl);
    };
    if &uri[..end] == "file" {
        return Err(PlatformError::InvalidUrl);
    }
    validate_uri_common(uri, &uri[..end])
}

fn validate_uri_common(uri: &str, scheme: &str) -> Result<ValidatedUrl, PlatformError> {
    if uri.is_empty() || uri.len() > URL_MAX_LEN || !uri.is_ascii() {
        return Err(PlatformError::InvalidUrl);
    }
    if uri
        .chars()
        .any(|ch| ch.is_ascii_control() || ch.is_ascii_whitespace())
    {
        return Err(PlatformError::InvalidUrl);
    }
    if !matches!(scheme, "http" | "https" | "mailto" | "file") {
        return Err(PlatformError::InvalidUrl);
    }
    let forbidden = |byte| {
        matches!(
            byte,
            b'\''
                | b'"'
                | b'`'
                | b';'
                | b'&'
                | b'|'
                | b'<'
                | b'>'
                | b'$'
                | b'('
                | b')'
                | b'!'
                | b'\\'
        )
    };
    let bytes = uri.as_bytes();
    let mut i = scheme.len() + 1;
    while i < bytes.len() {
        if forbidden(bytes[i]) {
            return Err(PlatformError::InvalidUrl);
        }
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return Err(PlatformError::InvalidUrl);
            }
            let decoded = u8::from_str_radix(&uri[i + 1..i + 3], 16).unwrap_or(0);
            if decoded.is_ascii_control() || forbidden(decoded) {
                return Err(PlatformError::InvalidUrl);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    Ok(ValidatedUrl(uri.to_owned()))
}

/// Validates an authority-free local file URI for activation.
pub fn validate_file_url(uri: &str) -> Result<ValidatedUrl, PlatformError> {
    if !uri.starts_with("file:///") || !is_local_file_url(uri) {
        Err(PlatformError::InvalidUrl)
    } else {
        validate_uri_common(uri, "file")
    }
}

/// Opens a validated URI using the platform's default handler.
///
/// The URI is passed as one argument to an executable; no shell or command
/// interpolation is involved. The child is not waited on, so the caller does
/// not block on an external handler.
///
/// Crate-private: external crates must go through the runtime's
/// `ActivationGesture` + `intercept_open_url` gate.
#[allow(dead_code)]
pub(crate) fn open_url(url: &ValidatedUrl) -> Result<(), PlatformError> {
    if url.as_str().starts_with("file:") {
        return Err(PlatformError::UrlActivationDenied);
    }
    spawn_url_handler(url)
}

/// Opens a validated local-file URI after a distinct file capability check.
///
/// Crate-private: file URLs require the distinct `FileUrlActivation` path.
#[allow(dead_code)]
pub(crate) fn open_file_url(url: &ValidatedUrl) -> Result<(), PlatformError> {
    if !is_local_file_url(url.as_str()) {
        return Err(PlatformError::InvalidUrl);
    }
    spawn_url_handler(url)
}

/// Accept only the local, authority-free form `file:///absolute/path`.
/// Remote authorities and encoded separators/traversal are rejected because
/// this boundary does not canonicalize paths or establish a filesystem sandbox.
fn is_local_file_url(uri: &str) -> bool {
    let Some(path) = uri.strip_prefix("file:///") else {
        return false;
    };
    let mut segment = Vec::new();
    for byte in path.bytes() {
        if byte == b'/' {
            if segment == b".." {
                return false;
            }
            segment.clear();
        } else {
            segment.push(byte);
        }
    }
    if segment == b".." {
        return false;
    }
    !uri.as_bytes().windows(3).any(|window| {
        window[0] == b'%'
            && matches!(window[1].to_ascii_lowercase(), b'2' | b'5')
            && matches!(window[2].to_ascii_lowercase(), b'e' | b'f' | b'c')
    })
}

#[allow(dead_code)]
fn spawn_url_handler(url: &ValidatedUrl) -> Result<(), PlatformError> {
    let (program, prefix) = url_dispatch();
    if cfg!(target_os = "linux") && !handler_available(program) {
        return Err(PlatformError::UrlLaunch(format!(
            "URL handler is unavailable: {program}"
        )));
    }
    let mut command = Command::new(program);
    if let Some(argument) = prefix {
        command.arg(argument);
    }
    command.arg(url.as_str());
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| PlatformError::UrlLaunch(error.to_string()))
}

#[allow(dead_code)]
fn handler_available(program: &str) -> bool {
    std::path::Path::new(program).is_file()
}

#[allow(dead_code)]
fn url_handler() -> &'static str {
    if cfg!(target_os = "windows") {
        r"C:\Windows\System32\explorer.exe"
    } else if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        // `gio` is a native binary, unlike the PATH-resolved xdg-open script.
        "/usr/bin/gio"
    }
}

#[allow(dead_code)]
fn url_dispatch() -> (&'static str, Option<&'static str>) {
    if cfg!(target_os = "linux") {
        (url_handler(), Some("open"))
    } else {
        (url_handler(), None)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::handler_available;
    use super::{is_local_file_url, url_dispatch, url_handler, validate_file_url, validate_url};

    #[test]
    fn adversarial_uri_corpus_is_rejected() {
        for uri in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java%73cript:alert(1)",
            "https://example.test/%3Btouch%20/tmp/x",
            "https://example.test;touch /tmp/x",
            "https://example.test/`id`",
            "https://example.test/$(id)",
            "https://example.test/$HOME",
            "https://example.test/\\x",
            "https://example.test/\nnext",
            "file:///tmp/a|b",
            "file://attacker/share",
            "file://server/share",
            "file://%2Ftmp/share",
            "https://example.test/%",
        ] {
            assert!(validate_url(uri).is_err(), "accepted hostile URI: {uri:?}");
        }
    }

    #[test]
    fn supported_uri_corpus_is_accepted() {
        for uri in [
            "https://example.test/path?q=a%20b",
            "http://127.0.0.1:8080/",
            "mailto:user@example.test",
        ] {
            assert!(validate_url(uri).is_ok(), "rejected supported URI: {uri:?}");
        }
    }

    #[test]
    fn handler_is_an_absolute_native_executable() {
        if cfg!(target_os = "windows") {
            assert_eq!(url_handler(), r"C:\Windows\System32\explorer.exe");
        } else {
            assert!(url_handler().starts_with('/'));
            assert!(!url_handler().contains("xdg-open"));
        }
    }

    #[test]
    fn dispatch_uses_separate_native_arguments() {
        let (program, prefix) = url_dispatch();
        assert_eq!(program, url_handler());
        if cfg!(target_os = "linux") {
            assert_eq!(prefix, Some("open"));
        } else {
            assert_eq!(prefix, None);
        }
    }

    #[test]
    fn validated_url_is_the_only_opener_input() {
        let url = validate_url("https://example.test").unwrap();
        assert_eq!(url.as_str(), "https://example.test");
    }

    #[test]
    fn generic_opener_cannot_activate_file_urls() {
        assert!(validate_url("file:///tmp/report.txt").is_err());
    }

    #[test]
    fn file_opener_requires_local_authority_free_form() {
        assert!(is_local_file_url("file:///tmp/report.txt"));
        for uri in [
            "file://attacker/share",
            "file://server/share",
            "file://%2Ftmp/share",
            "file:///tmp/%2e%2e/etc/passwd",
            "file:///tmp/%2Fetc/passwd",
        ] {
            assert!(
                !is_local_file_url(uri),
                "accepted non-local file URI: {uri:?}"
            );
        }
    }

    #[test]
    fn file_validation_rejects_remote_and_traversal_authorities() {
        for uri in [
            "file://attacker/share",
            "file://server/share",
            "file://%2Ftmp/share",
            "file:///tmp/../etc/passwd",
            "file:///tmp/%2e%2e/etc/passwd",
        ] {
            assert!(
                validate_file_url(uri).is_err(),
                "accepted unsafe file URI: {uri:?}"
            );
        }
    }

    #[test]
    fn generic_validation_cannot_issue_file_capability() {
        for uri in ["file:///tmp/report.txt", "file:///tmp/../etc/passwd"] {
            assert!(validate_url(uri).is_err());
        }
        assert!(validate_file_url("file:///tmp/report.txt").is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_linux_handler_is_reported_before_spawn() {
        assert!(!handler_available("/definitely/missing/bitty-gio"));
    }
}
