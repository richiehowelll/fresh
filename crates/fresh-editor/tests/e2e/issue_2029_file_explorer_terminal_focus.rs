//! Regression test for issue #2029 (sub-issue 1: file-explorer focus is
//! stolen back to the terminal).
//!
//! When the user is in an active terminal (`terminal_mode = true`) and
//! transfers focus to the file explorer — either by toggling it with
//! `Ctrl+B` or by clicking a file inside it — `terminal_mode` is left
//! stale. Because `dispatch_terminal_input` only checks the flag and
//! not `key_context`, the user's next keystroke is still forwarded to
//! the PTY even though the explorer is visually focused.
//!
//! Two reproductions covered:
//!
//! 1a. `Ctrl+B` while a terminal is active: the explorer opens, the
//!     "Explorer" menu becomes visible, status says "File explorer
//!     opened" — but Up/Down navigates bash history in the terminal
//!     instead of moving the file-explorer selection.
//!
//! 1b. Click on a file in the explorer while a terminal is active:
//!     per the docstring at `click_handlers.rs:554-557`, a single
//!     click should "Open the file but keep focus on file explorer".
//!     Today the click handler's `key_context = FileExplorer` write
//!     is undone by `set_active_buffer` (`active_focus.rs:103-107`),
//!     which resets `key_context = Normal` because we were leaving a
//!     terminal buffer.
//!
//! Observability: per CONTRIBUTING §Testing, each test drives
//! keyboard / mouse input and asserts purely on rendered screen
//! content. The bug's user-visible signal is "Down doesn't drive
//! the file explorer" — verified by waiting for the previewed
//! file's contents to appear on screen. If keys leaked to the PTY,
//! that content would never reach the screen and the test would
//! time out externally.

use crate::common::harness::EditorTestHarness;
use crossterm::event::{KeyCode, KeyModifiers};
use portable_pty::{native_pty_system, PtySize};
use std::fs;

fn pty_available() -> bool {
    native_pty_system()
        .openpty(PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

fn explorer_row_for(harness: &EditorTestHarness, name: &str) -> u16 {
    let screen = harness.screen_to_string();
    const FIRST_EXPLORER_ROW: usize = 2;
    for (row, line) in screen.lines().enumerate().skip(FIRST_EXPLORER_ROW) {
        let prefix: String = line.chars().take(40).collect();
        if prefix.contains(name) {
            return row as u16;
        }
    }
    panic!("file {name} not found in file explorer;\nscreen:\n{screen}");
}

/// 1a — `Ctrl+B` from an active terminal must transfer focus to the
/// file explorer in a way that subsequent arrow keys reach the
/// explorer, not the underlying terminal PTY.
///
/// Pure screen-observable assertion: after `Ctrl+B` opens the
/// explorer and `Down` selects `alpha.txt`, the explorer's preview
/// flow must open the file and the file's content must appear on
/// screen. If keys still routed to the PTY, the screen would never
/// gain that text.
#[test]
fn ctrl_b_from_terminal_transfers_keyboard_focus_to_file_explorer() {
    if !pty_available() {
        eprintln!("Skipping: PTY not available in this environment");
        return;
    }

    let mut harness = EditorTestHarness::with_temp_project(120, 40).unwrap();
    let project = harness.project_dir().unwrap();
    fs::write(project.join("alpha.txt"), "ALPHA_FILE_CONTENT\n").unwrap();

    // Wait for the terminal to actually render (its tab text appears
    // in the tab bar) before sending Ctrl+B. Without this gate, on
    // heavily-loaded CI the Ctrl+B can race ahead of the terminal's
    // own async setup and the binding resolves against a transient
    // pre-terminal context.
    harness.editor_mut().open_terminal();
    harness.wait_for_screen_contains("*Terminal 0*").unwrap();

    // Ctrl+B opens the file explorer. Wait for both the panel and
    // the target item to render so the subsequent Down is observed
    // against a fully-initialized tree.
    harness
        .send_key(KeyCode::Char('b'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_file_explorer().unwrap();
    harness.wait_for_file_explorer_item("alpha.txt").unwrap();

    // Down selects the next item in the explorer and triggers a
    // preview open (preview_tabs defaults to true). The preview's
    // content reaching the screen is the user-visible signal that
    // the keypress drove the explorer, not the underlying PTY.
    harness.send_key(KeyCode::Down, KeyModifiers::NONE).unwrap();
    harness
        .wait_for_screen_contains("ALPHA_FILE_CONTENT")
        .unwrap();
}

/// 1b — single-clicking a file in the explorer while a terminal is the
/// active buffer must keep keyboard focus on the file explorer, so the
/// user can keep arrow-browsing previews. Today focus ends up on the
/// previewed editor buffer.
///
/// Screen-observable test: after a click on `alpha.txt`, pressing
/// `Down` should advance the explorer selection to `beta.txt` and
/// trigger its preview — `beta.txt`'s content must appear on screen.
/// With focus stolen to the editor (the bug), `Down` would move the
/// cursor inside the alpha.txt buffer and `BETA_FILE_CONTENT` would
/// never appear.
#[test]
fn click_in_explorer_while_terminal_active_keeps_focus_on_explorer() {
    if !pty_available() {
        eprintln!("Skipping: PTY not available in this environment");
        return;
    }

    let mut harness = EditorTestHarness::with_temp_project(120, 40).unwrap();
    let project = harness.project_dir().unwrap();
    fs::write(project.join("alpha.txt"), "ALPHA_FILE_CONTENT\n").unwrap();
    fs::write(project.join("beta.txt"), "BETA_FILE_CONTENT\n").unwrap();

    // Same precondition wait as 1a — terminal fully rendered before
    // we send any keys against it.
    harness.editor_mut().open_terminal();
    harness.wait_for_screen_contains("*Terminal 0*").unwrap();

    // Open the file explorer. The 1a-style fix clears `terminal_mode`
    // here; this 1b test stresses what happens *after* — a single
    // click on a file must keep focus on the explorer rather than
    // handing it to the previewed buffer.
    harness
        .send_key(KeyCode::Char('b'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_file_explorer().unwrap();
    harness.wait_for_file_explorer_item("alpha.txt").unwrap();
    harness.wait_for_file_explorer_item("beta.txt").unwrap();

    // Single-click alpha.txt. Wait for the preview to render so the
    // next keypress is observed against a settled UI.
    let alpha_row = explorer_row_for(&harness, "alpha.txt");
    harness.mouse_click(10, alpha_row).unwrap();
    harness
        .wait_for_screen_contains("ALPHA_FILE_CONTENT")
        .unwrap();

    // Down should advance the *explorer* selection to beta.txt and
    // preview it. If focus leaked to the previewed editor buffer
    // (the bug), Down would move the cursor inside alpha.txt and
    // `BETA_FILE_CONTENT` would never appear.
    harness.send_key(KeyCode::Down, KeyModifiers::NONE).unwrap();
    harness
        .wait_for_screen_contains("BETA_FILE_CONTENT")
        .unwrap();
}
