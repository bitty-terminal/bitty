//! Windows GUI-subsystem and console-output proof (bitty#2929).
//!
//! `bitty.exe` links with the Windows GUI subsystem so a Start Menu/Search
//! launch never allocates a console window (Windows Terminal) before the Bitty
//! window appears. That changes both the PE header and the console the process
//! inherits, so both are asserted on the real artifact this test run builds:
//!
//! 1. `IMAGE_OPTIONAL_HEADER.Subsystem` is `IMAGE_SUBSYSTEM_WINDOWS_GUI` (2) —
//!    the no-console-flash property, read straight from the executable;
//! 2. `bitty --version` still writes to a stream the parent wired (the pipe
//!    this test captures), so the startup console adoption preserves redirected
//!    output instead of discarding it.
//!
//! The whole file is `#![cfg(windows)]`: there is no PE header elsewhere. The
//! interactive parent-console path (a real PowerShell/cmd session) is covered
//! by the workflow step `Assert GUI subsystem and console output`; a GUI launch
//! with no console at all still needs a real desktop run.

#![cfg(windows)]

/// Binary under test (compile-time proof the artifact is named `bitty`).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// `IMAGE_SUBSYSTEM_WINDOWS_GUI` from the PE Optional Header.
const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;

/// Offset of `e_lfanew` (the PE header file offset) in the DOS header.
const DOS_LFANEW_OFFSET: usize = 0x3C;

/// Size of the PE signature (4 bytes) plus the COFF file header (20 bytes).
const PE_SIGNATURE_AND_FILE_HEADER: usize = 24;

/// Offset of `Subsystem` inside `IMAGE_OPTIONAL_HEADER`, identical for PE32
/// and PE32+ (the field sits before the image-base/stack layout differences).
const OPTIONAL_HEADER_SUBSYSTEM_OFFSET: usize = 68;

/// Reads a little-endian `u16` at `offset`, failing loudly past the end.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let slice = bytes
        .get(offset..offset + 2)
        .unwrap_or_else(|| panic!("PE image ends before offset {offset}"));
    u16::from_le_bytes([slice[0], slice[1]])
}

/// Reads a little-endian `i32` at `offset`, failing loudly past the end.
fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    let slice = bytes
        .get(offset..offset + 4)
        .unwrap_or_else(|| panic!("PE image ends before offset {offset}"));
    i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
}

#[test]
fn binary_links_with_the_windows_gui_subsystem() {
    let bytes = std::fs::read(BITTY_BIN).expect("read the bitty executable");
    let pe_offset = read_i32(&bytes, DOS_LFANEW_OFFSET);
    assert!(pe_offset > 0, "DOS header carries a PE offset: {pe_offset}");
    let pe_offset = pe_offset as usize;
    assert_eq!(
        bytes.get(pe_offset..pe_offset + 4),
        Some(b"PE\0\0".as_slice()),
        "not a PE image: {BITTY_BIN}"
    );
    let subsystem = read_u16(
        &bytes,
        pe_offset + PE_SIGNATURE_AND_FILE_HEADER + OPTIONAL_HEADER_SUBSYSTEM_OFFSET,
    );
    assert_eq!(
        subsystem, IMAGE_SUBSYSTEM_WINDOWS_GUI,
        "bitty.exe must link as a GUI-subsystem binary (bitty#2929): a console \
         subsystem allocates Windows Terminal before the Bitty window appears"
    );
}

#[test]
fn version_prints_through_a_parent_wired_stream() {
    // A pipe is a handle the parent wired, so startup must leave it alone: this
    // is the same contract as `bitty --version > out.txt` in a shell.
    let output = std::process::Command::new(BITTY_BIN)
        .arg("--version")
        .output()
        .expect("run bitty --version");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bitty --version exited {:?}: stdout={stdout:?} stderr={stderr:?}",
        output.status
    );
    assert!(
        stdout.contains("bitty"),
        "bitty --version printed nothing usable: stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn help_prints_through_a_parent_wired_stream() {
    let output = std::process::Command::new(BITTY_BIN)
        .arg("--help")
        .output()
        .expect("run bitty --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "bitty --help exited {:?}",
        output.status
    );
    assert!(
        stdout.contains("bitty"),
        "bitty --help printed nothing usable: {stdout:?}"
    );
}
