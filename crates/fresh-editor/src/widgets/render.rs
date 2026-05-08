//! Render a `WidgetSpec` tree into `Vec<TextPropertyEntry>`.
//!
//! This is the path from declarative spec to the bytes the existing
//! virtual-buffer pipeline already knows how to display. By going
//! through `TextPropertyEntry`, widgets paint via exactly the same
//! renderer that today's `setVirtualBufferContent` uses — no parallel
//! render path. This is what makes the new widget API additive: the
//! buffer mid-bytes are indistinguishable from hand-rolled output.
//!
//! v1 dispatches on four kinds:
//!   * `Row` — children laid out left-to-right within a single line
//!     (the result is one `TextPropertyEntry`).
//!   * `Col` — children stacked vertically (the result is one
//!     `TextPropertyEntry` per child output line).
//!   * `HintBar` — keyboard-hint footer (one `TextPropertyEntry`).
//!   * `Raw` — pass-through (zero interpretation; plugin's entries
//!     flow through unchanged).
//!
//! Future kinds (`Toggle`, `Button`, `TextInput`, `List`, `Tree`,
//! `Layer`, `Transient`, `Table`) extend the dispatch without
//! changing the public function signature.

use crate::widgets::registry::{HitArea, WidgetInstanceState};
use fresh_core::api::{ButtonKind, HintEntry, OverlayColorSpec, OverlayOptions, WidgetSpec};
use fresh_core::text_property::{InlineOverlay, TextPropertyEntry};
use serde_json::json;
use std::collections::HashMap;

// Theme keys used by the v1 widget renderers. Centralized so future
// "role-based" theming (§7 of the design doc) has one place to
// substitute the role→key mapping.
const KEY_HELP_KEY_FG: &str = "ui.help_key_fg";
const KEY_TOGGLE_ON_FG: &str = "ui.tab_active_fg";
const KEY_FOCUSED_FG: &str = "ui.menu_active_fg";
const KEY_FOCUSED_BG: &str = "ui.menu_active_bg";
const KEY_DANGER_FG: &str = "ui.status_error_indicator_fg";
const KEY_INPUT_BG: &str = "ui.prompt_bg";
const KEY_PLACEHOLDER_FG: &str = "ui.menu_disabled_fg";
const KEY_CURSOR_BG: &str = "editor.cursor";

/// What a single render of a `WidgetSpec` produces.
///
/// * `entries` — the bytes for `set_virtual_buffer_content`.
/// * `hits` — click rectangles for the `WidgetRegistry` so a later
///   `mouse_click` dispatches a semantic `widget_event`.
/// * `instance_states` — next-tick widget instance state (List
///   scroll offsets in v1; TextInput cursor / Tree expanded keys
///   later).
/// * `focus_key` — currently focused widget key, clamped to a
///   tabbable that exists in the spec (or `""` when there are no
///   tabbables).
/// * `tabbable` — focusable widget keys collected in declaration
///   order. The Tab-cycle command finds the current `focus_key`'s
///   index in this list to advance it.
pub struct RenderOutput {
    pub entries: Vec<TextPropertyEntry>,
    pub hits: Vec<HitArea>,
    pub instance_states: HashMap<String, WidgetInstanceState>,
    pub focus_key: String,
    pub tabbable: Vec<String>,
}

/// Render a spec to a [`RenderOutput`].
///
/// `prev` is the previous render's instance state (or empty on
/// first mount). `prev_focus_key` is the previous render's focus
/// key (or `""`); the renderer keeps it if it matches a tabbable in
/// the new spec, otherwise falls back to the first tabbable.
pub fn render_spec(
    spec: &WidgetSpec,
    prev: &HashMap<String, WidgetInstanceState>,
    prev_focus_key: &str,
) -> RenderOutput {
    // Walk the spec to collect tabbable keys, then resolve the
    // active focus key. This must happen before the entry pass so
    // that widget arms know whether they're focused.
    let mut tabbable = Vec::new();
    collect_tabbable(spec, &mut tabbable);
    let focus_key = if !prev_focus_key.is_empty()
        && tabbable.iter().any(|k| k == prev_focus_key)
    {
        prev_focus_key.to_string()
    } else {
        tabbable.first().cloned().unwrap_or_default()
    };

    let mut next_state = HashMap::new();
    let (entries, hits) =
        render_collected(spec, prev, &mut next_state, &focus_key);
    RenderOutput {
        entries,
        hits,
        instance_states: next_state,
        focus_key,
        tabbable,
    }
}

/// Walk a spec tree and append tabbable widget keys (`Toggle`,
/// `Button`, `TextInput`, `List` with a non-empty `key`) in
/// declaration order. Layout containers (`Row`, `Col`) recurse;
/// `Raw`, `Spacer`, `HintBar` skip.
fn collect_tabbable(spec: &WidgetSpec, out: &mut Vec<String>) {
    match spec {
        WidgetSpec::Row { children, .. } | WidgetSpec::Col { children, .. } => {
            for c in children {
                collect_tabbable(c, out);
            }
        }
        WidgetSpec::Toggle { key: Some(k), .. }
        | WidgetSpec::Button { key: Some(k), .. }
        | WidgetSpec::TextInput { key: Some(k), .. }
        | WidgetSpec::List { key: Some(k), .. }
            if !k.is_empty() =>
        {
            out.push(k.clone());
        }
        _ => {}
    }
}

/// Internal renderer. Returns the entries and the hit areas
/// produced by `spec` *as if* it were rendered at row 0; callers
/// (Col, Row block path) shift `buffer_row` upward by their own
/// row offset before forwarding. `prev` is read-only previous
/// instance state; `next_state` accumulates the post-render state
/// the host should persist. `focus_key` is the panel's currently
/// focused widget key — widget arms compare against their own
/// `key` to decide whether to render with focus styling, ignoring
/// the spec's `focused` field. (Plugin-passed `focused` is the
/// initial-only hint that becomes redundant once the host's focus
/// key takes over.)
fn render_collected(
    spec: &WidgetSpec,
    prev: &HashMap<String, WidgetInstanceState>,
    next_state: &mut HashMap<String, WidgetInstanceState>,
    focus_key: &str,
) -> (Vec<TextPropertyEntry>, Vec<HitArea>) {
    let mut entries: Vec<TextPropertyEntry> = Vec::new();
    let mut hits: Vec<HitArea> = Vec::new();
    match spec {
        WidgetSpec::Row { children, .. } => {
            // Rows collapse inline-sized children into a single
            // `TextPropertyEntry`. Children that emit multiple lines
            // (e.g. nested Col, Raw with several entries) flush the
            // accumulator and pass through. Hit areas from inline
            // children share the merged row; their byte offsets
            // shift by the merged-text length so far. Block
            // children's hits keep their own row index, biased by
            // the number of entries already emitted.
            let mut acc: Option<TextPropertyEntry> = None;
            for child in children {
                let (child_entries, child_hits) =
                    render_collected(child, prev, next_state, focus_key);
                if child_entries.is_empty() {
                    debug_assert!(child_hits.is_empty(), "empty children produce no hits");
                    continue;
                }
                if child_entries.len() == 1 {
                    let mut child_entry = child_entries.into_iter().next().unwrap();
                    let inline_shift = match acc.as_ref() {
                        Some(e) => e.text.len(),
                        None => 0,
                    };
                    for mut h in child_hits {
                        // Inline child's hits all collapse onto the
                        // accumulator's row; byte ranges shift by the
                        // text length we've already merged.
                        h.byte_start += inline_shift;
                        h.byte_end += inline_shift;
                        // buffer_row stays at 0 — caller (Col / top
                        // level) will rebase it.
                        hits.push(h);
                    }
                    match acc.as_mut() {
                        Some(merged) => merge_inline(merged, &mut child_entry),
                        None => acc = Some(child_entry),
                    }
                } else {
                    // Multi-line child: flush the accumulator and
                    // emit the block. Hits from the block keep their
                    // own row index relative to the block's first
                    // line, plus the row offset of where the block
                    // lands in `entries`.
                    if let Some(merged) = acc.take() {
                        entries.push(merged);
                    }
                    let row_offset = entries.len() as u32;
                    for mut h in child_hits {
                        h.buffer_row += row_offset;
                        hits.push(h);
                    }
                    entries.extend(child_entries);
                }
            }
            if let Some(merged) = acc {
                entries.push(merged);
            }
        }
        WidgetSpec::Col { children, .. } => {
            for child in children {
                let (child_entries, child_hits) =
                    render_collected(child, prev, next_state, focus_key);
                let row_offset = entries.len() as u32;
                for mut h in child_hits {
                    h.buffer_row += row_offset;
                    hits.push(h);
                }
                entries.extend(child_entries);
            }
        }
        WidgetSpec::HintBar {
            entries: hint_entries,
            ..
        } => {
            entries.push(render_hint_bar(hint_entries));
            // No hits — HintBar is read-only in v1. (When the
            // keymap layer arrives, individual entries become
            // clickable command targets.)
        }
        WidgetSpec::Toggle {
            checked,
            label,
            focused,
            key,
        } => {
            // Host-managed focus overrides the spec's `focused`
            // when this widget has a key and is the panel's focused
            // widget. Plugin-passed `focused` is ignored when the
            // host owns focus (i.e. the panel has any tabbable
            // widgets); without it, the renderer falls back to the
            // spec value (legacy path).
            let is_focused = match key.as_deref() {
                Some(k) if !k.is_empty() => k == focus_key,
                _ => *focused,
            };
            let entry = render_toggle(*checked, label, is_focused);
            let byte_end = entry.text.len();
            hits.push(HitArea {
                widget_key: key.clone().unwrap_or_default(),
                widget_kind: "toggle",
                buffer_row: 0,
                byte_start: 0,
                byte_end,
                payload: json!({ "checked": !*checked }),
                event_type: "toggle",
            });
            entries.push(entry);
        }
        WidgetSpec::Button {
            label,
            focused,
            intent,
            key,
        } => {
            let is_focused = match key.as_deref() {
                Some(k) if !k.is_empty() => k == focus_key,
                _ => *focused,
            };
            let entry = render_button(label, is_focused, *intent);
            let byte_end = entry.text.len();
            hits.push(HitArea {
                widget_key: key.clone().unwrap_or_default(),
                widget_kind: "button",
                buffer_row: 0,
                byte_start: 0,
                byte_end,
                payload: json!({}),
                event_type: "activate",
            });
            entries.push(entry);
        }
        WidgetSpec::Spacer { cols, .. } => {
            // In an inline-row context a Spacer is N spaces; in a
            // block context (top-level / Col) it's a short blank
            // line. Either way: one entry, no hit areas.
            let cols = (*cols).min(4096) as usize;
            let mut text = String::with_capacity(cols);
            for _ in 0..cols {
                text.push(' ');
            }
            entries.push(TextPropertyEntry {
                text,
                properties: Default::default(),
                style: None,
                inline_overlays: Vec::new(),
            });
        }
        WidgetSpec::List {
            items,
            item_keys,
            selected_index,
            visible_rows,
            key: list_key,
        } => {
            // Look up host-owned scroll + selected index from prev
            // state (becomes authoritative after first render).
            // Spec's `selected_index` is initial-only on first
            // mount; subsequent updates read instance state.
            let total = items.len() as u32;
            let visible = (*visible_rows).max(1);
            let (prev_scroll, prev_sel) = list_key
                .as_deref()
                .and_then(|k| prev.get(k))
                .and_then(|s| match s {
                    WidgetInstanceState::List {
                        scroll_offset,
                        selected_index,
                    } => Some((*scroll_offset, *selected_index)),
                    _ => None,
                })
                .unwrap_or((0, *selected_index));
            // Clamp the previous selection to the current dataset
            // size — items may have shrunk between renders (e.g.
            // search results changed). Out-of-range selections
            // collapse to the last item, or -1 if the list is
            // now empty.
            let effective_sel = if prev_sel < 0 {
                -1
            } else if total == 0 {
                -1
            } else if (prev_sel as u32) >= total {
                (total - 1) as i32
            } else {
                prev_sel
            };

            // Compute scroll: auto-clamp to keep selection in view
            // and never extend past the dataset end.
            let mut scroll = prev_scroll;
            if effective_sel >= 0 {
                let sel = effective_sel as u32;
                if sel < scroll {
                    scroll = sel;
                }
                if sel >= scroll + visible {
                    scroll = sel + 1 - visible;
                }
            }
            let max_scroll = total.saturating_sub(visible);
            if scroll > max_scroll {
                scroll = max_scroll;
            }
            // Persist scroll + selection for the next render.
            // Lists without a `key` lose state across updates.
            if let Some(k) = list_key.as_deref() {
                next_state.insert(
                    k.to_string(),
                    WidgetInstanceState::List {
                        scroll_offset: scroll,
                        selected_index: effective_sel,
                    },
                );
            }

            // Render the visible window, emitting one entry + one
            // hit area per visible item. Selected row gets the
            // menu_active_bg + extend_to_line_end style. Hit-area
            // payload uses the *absolute* item index so the plugin
            // never needs to translate window-relative coordinates.
            let start = scroll as usize;
            let end = ((scroll + visible) as usize).min(items.len());
            for i in start..end {
                let mut entry = items[i].clone();
                let is_selected = i as i32 == effective_sel;
                if is_selected {
                    let mut style = entry.style.unwrap_or_default();
                    style.bg = Some(OverlayColorSpec::theme_key(KEY_FOCUSED_BG));
                    style.extend_to_line_end = true;
                    entry.style = Some(style);
                }
                let byte_end = entry.text.len();
                entries.push(entry);
                let item_key = item_keys.get(i).cloned().unwrap_or_default();
                let hit_row = (entries.len() - 1) as u32;
                hits.push(HitArea {
                    widget_key: item_key.clone(),
                    widget_kind: "list",
                    buffer_row: hit_row,
                    byte_start: 0,
                    byte_end,
                    payload: json!({
                        "index": i as i64,
                        "key": item_key,
                    }),
                    event_type: "select",
                });
            }
        }
        WidgetSpec::TextInput {
            value,
            cursor_byte,
            focused,
            label,
            placeholder,
            max_visible_chars,
            key,
        } => {
            let is_focused = match key.as_deref() {
                Some(k) if !k.is_empty() => k == focus_key,
                _ => *focused,
            };
            // Host-owned value/cursor: read instance state if it
            // exists; else seed from spec on first render. This is
            // what makes concurrent keystroke dispatch correct —
            // see WidgetInstanceState::TextInput doc.
            let (effective_value, effective_cursor_byte) = match key
                .as_deref()
                .filter(|k| !k.is_empty())
                .and_then(|k| prev.get(k))
            {
                Some(WidgetInstanceState::TextInput { value, cursor_byte }) => {
                    (value.clone(), *cursor_byte as i32)
                }
                _ => (value.clone(), *cursor_byte),
            };
            if let Some(k) = key.as_deref().filter(|k| !k.is_empty()) {
                let cb = effective_cursor_byte
                    .max(0)
                    .min(effective_value.len() as i32) as u32;
                next_state.insert(
                    k.to_string(),
                    WidgetInstanceState::TextInput {
                        value: effective_value.clone(),
                        cursor_byte: cb,
                    },
                );
            }
            // When focus moves away from a TextInput, hide the
            // cursor — the spec's `cursor_byte` stays around for
            // the plugin's bookkeeping but visually a non-focused
            // input shouldn't display a cursor.
            let effective_cursor = if is_focused {
                effective_cursor_byte
            } else {
                -1
            };
            entries.push(render_text_input(
                &effective_value,
                effective_cursor,
                is_focused,
                label,
                placeholder.as_deref(),
                *max_visible_chars,
            ));
            // No hit area in v1 — clicks on a TextInput will land
            // somewhere inside the bracketed value, but cursor
            // placement on click requires cursor mutation, which
            // needs the keymap-routing layer.
        }
        WidgetSpec::Raw {
            entries: raw_entries,
            ..
        } => {
            // Raw is the migration escape hatch: the plugin's own
            // bytes flow through unchanged. The plugin still owns
            // mouse clicks within Raw regions (via the existing
            // `mouse_click` hook); the widget runtime intentionally
            // emits no hit areas here.
            entries.extend(raw_entries.iter().cloned());
        }
    }
    (entries, hits)
}

/// Render a HintBar into a single `TextPropertyEntry`.
///
/// Layout: `<keys> <label>  <keys> <label>  …`. The key portion of
/// each entry is highlighted with the `ui.help_key_fg` theme key;
/// labels use the buffer's default foreground.
///
/// This replaces the per-plugin hand-rolled footer at e.g.
/// `crates/fresh-editor/plugins/search_replace.ts:535–541`,
/// `audit_mode.ts:1068–1158`, `pkg.ts:2136–2145`.
pub fn render_hint_bar(entries: &[HintEntry]) -> TextPropertyEntry {
    let separator = "  ";
    let mut text = String::new();
    let mut overlays = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            text.push_str(separator);
        }
        let key_start = text.len();
        text.push_str(&entry.keys);
        let key_end = text.len();
        if key_end > key_start {
            overlays.push(InlineOverlay {
                start: key_start,
                end: key_end,
                style: OverlayOptions {
                    fg: Some(OverlayColorSpec::theme_key(KEY_HELP_KEY_FG)),
                    bold: true,
                    ..Default::default()
                },
                properties: Default::default(),
            });
        }
        if !entry.label.is_empty() {
            text.push(' ');
            text.push_str(&entry.label);
        }
    }
    TextPropertyEntry {
        text,
        properties: Default::default(),
        style: None,
        inline_overlays: overlays,
    }
}

/// Render a `Toggle` to a single `TextPropertyEntry`.
///
/// Layout: `[v] label` when checked, `[ ] label` when not. The check
/// glyph is colored via `ui.tab_active_fg` when checked (no override
/// when unchecked). When focused, the entire entry is given a focused
/// fg/bg pair (`ui.menu_active_fg`/`ui.menu_active_bg`) plus bold —
/// matching the Settings UI's selected-control affordance.
pub fn render_toggle(checked: bool, label: &str, focused: bool) -> TextPropertyEntry {
    let glyph = if checked { "[v]" } else { "[ ]" };
    let mut text = String::with_capacity(glyph.len() + 1 + label.len());
    text.push_str(glyph);
    text.push(' ');
    text.push_str(label);

    let mut overlays = Vec::new();

    // Check-glyph color (only when checked — leaves default fg
    // when unchecked, which is what plugins do today).
    if checked {
        overlays.push(InlineOverlay {
            start: 0,
            end: glyph.len(),
            style: OverlayOptions {
                fg: Some(OverlayColorSpec::theme_key(KEY_TOGGLE_ON_FG)),
                bold: true,
                ..Default::default()
            },
            properties: Default::default(),
        });
    }

    // Focused: full-entry fg/bg + bold.
    if focused {
        overlays.push(InlineOverlay {
            start: 0,
            end: text.len(),
            style: OverlayOptions {
                fg: Some(OverlayColorSpec::theme_key(KEY_FOCUSED_FG)),
                bg: Some(OverlayColorSpec::theme_key(KEY_FOCUSED_BG)),
                bold: true,
                ..Default::default()
            },
            properties: Default::default(),
        });
    }

    TextPropertyEntry {
        text,
        properties: Default::default(),
        style: None,
        inline_overlays: overlays,
    }
}

/// Render a `Button` to a single `TextPropertyEntry`.
///
/// Layout: `[ Label ]` (with explicit space padding so the label
/// is visually inset from the brackets). Styling depends on `kind`
/// and `focused`:
///
/// * `Normal`     — default fg; focused → fg/bg flip + bold.
/// * `Primary`    — bold; focused → fg/bg flip.
/// * `Danger`     — red fg (theme `ui.status_error_indicator_fg`);
///                  focused → bold.
pub fn render_button(label: &str, focused: bool, kind: ButtonKind) -> TextPropertyEntry {
    let text = format!("[ {} ]", label);
    let mut overlays = Vec::new();

    let base_style = match kind {
        ButtonKind::Normal => OverlayOptions::default(),
        ButtonKind::Primary => OverlayOptions {
            bold: true,
            ..Default::default()
        },
        ButtonKind::Danger => OverlayOptions {
            fg: Some(OverlayColorSpec::theme_key(KEY_DANGER_FG)),
            ..Default::default()
        },
    };

    let style = if focused {
        OverlayOptions {
            fg: Some(OverlayColorSpec::theme_key(KEY_FOCUSED_FG)),
            bg: Some(OverlayColorSpec::theme_key(KEY_FOCUSED_BG)),
            bold: true,
            ..base_style
        }
    } else {
        base_style
    };

    // Only emit an overlay if the style is non-default — keeps the
    // serialized entry tight.
    if style.fg.is_some()
        || style.bg.is_some()
        || style.bold
        || style.italic
        || style.underline
        || style.strikethrough
    {
        overlays.push(InlineOverlay {
            start: 0,
            end: text.len(),
            style,
            properties: Default::default(),
        });
    }

    TextPropertyEntry {
        text,
        properties: Default::default(),
        style: None,
        inline_overlays: overlays,
    }
}

/// Render a `TextInput` to a single `TextPropertyEntry`.
///
/// Layout: `Label: [<value>]` or just `[<value>]` when `label` is
/// empty. When `value` is empty and the input is unfocused, the
/// `placeholder` (if provided) is shown in `ui.menu_disabled_fg`.
///
/// Cursor: when `cursor_byte >= 0`, a one-cell reverse-video overlay
/// is placed at the requested byte offset within `value`. If the
/// cursor sits past the last character, it highlights the closing
/// bracket — matching the pre-widget hand-rolled behaviour the
/// search_replace plugin relied on.
///
/// Focused state: the value range gets the input-bg theme key
/// (`ui.prompt_bg`) so the field visually reads as the active
/// editing target.
///
/// Truncation: when `max_visible_chars > 0` and `value` exceeds it,
/// the shown text is `…value-tail`, with the cursor still tracking
/// its logical byte position relative to the original value (best
/// effort — the displayed cursor approximates the truncated form).
pub fn render_text_input(
    value: &str,
    cursor_byte: i32,
    focused: bool,
    label: &str,
    placeholder: Option<&str>,
    max_visible_chars: u32,
) -> TextPropertyEntry {
    let show_placeholder = !focused && value.is_empty() && placeholder.is_some();

    // Decide what text goes inside the brackets.
    let inner: String = if show_placeholder {
        placeholder.unwrap_or("").to_string()
    } else if max_visible_chars > 0 && value.chars().count() > max_visible_chars as usize {
        // Tail-truncate so the cursor (typically at the end while
        // typing) stays visible.
        let chars: Vec<char> = value.chars().collect();
        let take = (max_visible_chars as usize).saturating_sub(1);
        let start = chars.len().saturating_sub(take);
        let tail: String = chars[start..].iter().collect();
        format!("…{}", tail)
    } else {
        value.to_string()
    };

    let mut text = String::new();
    if !label.is_empty() {
        text.push_str(label);
        text.push(' ');
    }
    let bracket_open_byte = text.len();
    text.push('[');
    let inner_byte_start = text.len();
    text.push_str(&inner);
    let inner_byte_end = text.len();
    text.push(']');
    let bracket_close_byte = text.len();

    let mut overlays = Vec::new();

    // Placeholder text: muted theme key, no other styling.
    if show_placeholder {
        overlays.push(InlineOverlay {
            start: inner_byte_start,
            end: inner_byte_end,
            style: OverlayOptions {
                fg: Some(OverlayColorSpec::theme_key(KEY_PLACEHOLDER_FG)),
                ..Default::default()
            },
            properties: Default::default(),
        });
    }

    // Focused: input-bg across the bracketed region (excluding the
    // brackets themselves, so the field reads as its own surface).
    if focused {
        overlays.push(InlineOverlay {
            start: bracket_open_byte,
            end: bracket_close_byte,
            style: OverlayOptions {
                bg: Some(OverlayColorSpec::theme_key(KEY_INPUT_BG)),
                ..Default::default()
            },
            properties: Default::default(),
        });
    }

    // Cursor: a single-grapheme reverse-video span at the cursor
    // byte position inside `value`. When the cursor is at end-of-
    // value (or past it), highlight the closing bracket.
    if cursor_byte >= 0 && !show_placeholder {
        let cb = cursor_byte as usize;
        let (start_byte, end_byte) = if cb >= inner.len() {
            // End-of-value cursor → highlight the closing bracket.
            (bracket_close_byte - 1, bracket_close_byte)
        } else {
            // Find next char boundary after the cursor.
            let mut next = cb + 1;
            while next < inner.len() && !inner.is_char_boundary(next) {
                next += 1;
            }
            (inner_byte_start + cb, inner_byte_start + next)
        };
        overlays.push(InlineOverlay {
            start: start_byte,
            end: end_byte,
            style: OverlayOptions {
                bg: Some(OverlayColorSpec::theme_key(KEY_CURSOR_BG)),
                ..Default::default()
            },
            properties: Default::default(),
        });
    }

    TextPropertyEntry {
        text,
        properties: Default::default(),
        style: None,
        inline_overlays: overlays,
    }
}

/// Merge `next` into `merged` for the inline-row collapse path.
/// `next`'s overlays are byte-shifted to account for the merged
/// text length so far.
fn merge_inline(merged: &mut TextPropertyEntry, next: &mut TextPropertyEntry) {
    let shift = merged.text.len();
    merged.text.push_str(&next.text);
    for overlay in next.inline_overlays.drain(..) {
        merged.inline_overlays.push(InlineOverlay {
            start: overlay.start + shift,
            end: overlay.end + shift,
            style: overlay.style,
            properties: overlay.properties,
        });
    }
    // `style` and `properties` from `next` are dropped — Row inline
    // collapse only preserves inline_overlays. Whole-entry style on
    // an inline-row child has no meaningful semantics here; if a
    // plugin needs whole-line styling it should produce a Col with
    // the styled child as its sole element.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most existing tests don't care about the new focus_key /
    /// tabbable fields. Wrap the no-focus-needed render path so
    /// they keep destructuring a 3-tuple; new tests destructure
    /// `RenderOutput` directly.
    fn render_no_focus(
        spec: &WidgetSpec,
        prev: &HashMap<String, WidgetInstanceState>,
    ) -> (
        Vec<TextPropertyEntry>,
        Vec<HitArea>,
        HashMap<String, WidgetInstanceState>,
    ) {
        let out = render_spec(spec, prev, "");
        (out.entries, out.hits, out.instance_states)
    }

    #[test]
    fn hint_bar_renders_entries_with_key_overlays() {
        let entries = vec![
            HintEntry {
                keys: "Tab".into(),
                label: "next".into(),
            },
            HintEntry {
                keys: "Esc".into(),
                label: "close".into(),
            },
        ];
        let entry = render_hint_bar(&entries);
        assert_eq!(entry.text, "Tab next  Esc close");
        assert_eq!(entry.inline_overlays.len(), 2);
        // First overlay covers "Tab" (bytes 0..3).
        assert_eq!(entry.inline_overlays[0].start, 0);
        assert_eq!(entry.inline_overlays[0].end, 3);
        // Second overlay covers "Esc" (bytes 10..13).
        assert_eq!(entry.inline_overlays[1].start, 10);
        assert_eq!(entry.inline_overlays[1].end, 13);
    }

    #[test]
    fn hint_bar_omits_label_when_empty() {
        let entries = vec![HintEntry {
            keys: "?".into(),
            label: "".into(),
        }];
        let entry = render_hint_bar(&entries);
        assert_eq!(entry.text, "?");
    }

    #[test]
    fn col_stacks_children_top_to_bottom() {
        let spec = WidgetSpec::Col {
            children: vec![
                WidgetSpec::HintBar {
                    entries: vec![HintEntry {
                        keys: "A".into(),
                        label: "alpha".into(),
                    }],
                    key: None,
                },
                WidgetSpec::HintBar {
                    entries: vec![HintEntry {
                        keys: "B".into(),
                        label: "beta".into(),
                    }],
                    key: None,
                },
            ],
            key: None,
        };
        let (out, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "A alpha");
        assert_eq!(out[1].text, "B beta");
        assert!(hits.is_empty(), "HintBar emits no hit areas in v1");
    }

    #[test]
    fn raw_passes_through_unchanged() {
        let spec = WidgetSpec::Raw {
            entries: vec![TextPropertyEntry::text("hello")],
            key: None,
        };
        let (out, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hello");
        assert!(hits.is_empty());
    }

    #[test]
    fn toggle_checked_emits_glyph_overlay() {
        let entry = render_toggle(true, "Case", false);
        assert_eq!(entry.text, "[v] Case");
        // One overlay for the glyph, no focused overlay.
        assert_eq!(entry.inline_overlays.len(), 1);
        assert_eq!(entry.inline_overlays[0].start, 0);
        assert_eq!(entry.inline_overlays[0].end, 3);
    }

    #[test]
    fn toggle_unchecked_no_glyph_overlay() {
        let entry = render_toggle(false, "Case", false);
        assert_eq!(entry.text, "[ ] Case");
        assert_eq!(entry.inline_overlays.len(), 0);
    }

    #[test]
    fn toggle_focused_adds_full_entry_overlay() {
        let entry = render_toggle(true, "Case", true);
        // Glyph overlay + focused overlay.
        assert_eq!(entry.inline_overlays.len(), 2);
        // Focused overlay spans the full entry.
        assert_eq!(entry.inline_overlays[1].start, 0);
        assert_eq!(entry.inline_overlays[1].end, entry.text.len());
        assert!(entry.inline_overlays[1].style.bold);
    }

    #[test]
    fn button_normal_unfocused_has_no_overlay() {
        let entry = render_button("Replace All", false, ButtonKind::Normal);
        assert_eq!(entry.text, "[ Replace All ]");
        assert!(entry.inline_overlays.is_empty());
    }

    #[test]
    fn button_primary_is_bold() {
        let entry = render_button("Submit", false, ButtonKind::Primary);
        assert_eq!(entry.inline_overlays.len(), 1);
        assert!(entry.inline_overlays[0].style.bold);
    }

    #[test]
    fn button_danger_uses_error_theme_key() {
        let entry = render_button("Delete", false, ButtonKind::Danger);
        assert_eq!(entry.inline_overlays.len(), 1);
        let fg = entry.inline_overlays[0].style.fg.as_ref().unwrap();
        assert_eq!(fg.as_theme_key(), Some("ui.status_error_indicator_fg"));
    }

    #[test]
    fn button_focused_overrides_with_menu_active_keys() {
        let entry = render_button("OK", true, ButtonKind::Normal);
        let style = &entry.inline_overlays[0].style;
        assert_eq!(
            style.fg.as_ref().and_then(|c| c.as_theme_key()),
            Some("ui.menu_active_fg")
        );
        assert_eq!(
            style.bg.as_ref().and_then(|c| c.as_theme_key()),
            Some("ui.menu_active_bg")
        );
        assert!(style.bold);
    }

    #[test]
    fn spacer_in_row_pads_with_spaces() {
        let spec = WidgetSpec::Row {
            children: vec![
                WidgetSpec::Toggle {
                    checked: false,
                    label: "A".into(),
                    focused: false,
                    key: None,
                },
                WidgetSpec::Spacer { cols: 4, key: None },
                WidgetSpec::Button {
                    label: "Go".into(),
                    focused: false,
                    intent: ButtonKind::Normal,
                    key: None,
                },
            ],
            key: None,
        };
        let (out, _hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "[ ] A    [ Go ]");
    }

    #[test]
    fn row_collapses_inline_children_with_shifted_overlays() {
        let spec = WidgetSpec::Row {
            children: vec![
                WidgetSpec::HintBar {
                    entries: vec![HintEntry {
                        keys: "Tab".into(),
                        label: "x".into(),
                    }],
                    key: None,
                },
                WidgetSpec::HintBar {
                    entries: vec![HintEntry {
                        keys: "Esc".into(),
                        label: "y".into(),
                    }],
                    key: None,
                },
            ],
            key: None,
        };
        let (out, _hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(out.len(), 1);
        // Two adjacent HintBars are concatenated; the second's overlay shifts.
        assert_eq!(out[0].text, "Tab xEsc y");
        assert_eq!(out[0].inline_overlays.len(), 2);
        assert_eq!(out[0].inline_overlays[1].start, 5);
        assert_eq!(out[0].inline_overlays[1].end, 8);
    }

    // -------------------------------------------------------------
    // Hit-area tests
    // -------------------------------------------------------------

    #[test]
    fn toggle_emits_hit_area_with_toggle_payload() {
        let spec = WidgetSpec::Toggle {
            checked: false,
            label: "Case".into(),
            focused: false,
            key: Some("case".into()),
        };
        let (_entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.widget_key, "case");
        assert_eq!(h.widget_kind, "toggle");
        assert_eq!(h.event_type, "toggle");
        assert_eq!(h.buffer_row, 0);
        assert_eq!(h.byte_start, 0);
        assert_eq!(h.byte_end, "[ ] Case".len());
        assert_eq!(h.payload, json!({"checked": true}));
    }

    #[test]
    fn button_emits_hit_area_with_activate_payload() {
        let spec = WidgetSpec::Button {
            label: "Replace All".into(),
            focused: false,
            intent: ButtonKind::Primary,
            key: Some("replace".into()),
        };
        let (_entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.widget_key, "replace");
        assert_eq!(h.widget_kind, "button");
        assert_eq!(h.event_type, "activate");
        assert_eq!(h.byte_end, "[ Replace All ]".len());
        assert_eq!(h.payload, json!({}));
    }

    #[test]
    fn row_inline_collapse_shifts_hit_byte_offsets() {
        let spec = WidgetSpec::Row {
            children: vec![
                WidgetSpec::Toggle {
                    checked: true,
                    label: "A".into(),
                    focused: false,
                    key: Some("a".into()),
                },
                WidgetSpec::Spacer { cols: 2, key: None },
                WidgetSpec::Toggle {
                    checked: false,
                    label: "B".into(),
                    focused: false,
                    key: Some("b".into()),
                },
            ],
            key: None,
        };
        let (entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        // One merged row with text "[v] A  [ ] B"
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "[v] A  [ ] B");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].widget_key, "a");
        assert_eq!(hits[0].buffer_row, 0);
        assert_eq!(hits[0].byte_start, 0);
        assert_eq!(hits[0].byte_end, 5); // "[v] A".len()
        // Second toggle shifts past first toggle ("[v] A".len() = 5)
        // + spacer ("  ".len() = 2) = 7.
        assert_eq!(hits[1].widget_key, "b");
        assert_eq!(hits[1].buffer_row, 0);
        assert_eq!(hits[1].byte_start, 7);
        assert_eq!(hits[1].byte_end, 12);
    }

    #[test]
    fn col_stacks_hit_rows() {
        let spec = WidgetSpec::Col {
            children: vec![
                WidgetSpec::Toggle {
                    checked: false,
                    label: "row0".into(),
                    focused: false,
                    key: Some("k0".into()),
                },
                WidgetSpec::Toggle {
                    checked: true,
                    label: "row1".into(),
                    focused: false,
                    key: Some("k1".into()),
                },
            ],
            key: None,
        };
        let (_entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].buffer_row, 0);
        assert_eq!(hits[1].buffer_row, 1);
    }

    // -------------------------------------------------------------
    // Focus management
    // -------------------------------------------------------------

    #[test]
    fn collect_tabbable_visits_widgets_with_keys_in_declaration_order() {
        let spec = WidgetSpec::Col {
            children: vec![
                WidgetSpec::HintBar {
                    entries: vec![],
                    key: Some("hb".into()),
                },
                WidgetSpec::Row {
                    children: vec![
                        WidgetSpec::Toggle {
                            checked: false,
                            label: "T".into(),
                            focused: false,
                            key: Some("t".into()),
                        },
                        WidgetSpec::Spacer { cols: 1, key: None },
                        WidgetSpec::Button {
                            label: "B".into(),
                            focused: false,
                            intent: ButtonKind::Normal,
                            key: Some("b".into()),
                        },
                    ],
                    key: None,
                },
                WidgetSpec::TextInput {
                    value: "".into(),
                    cursor_byte: -1,
                    focused: false,
                    label: "".into(),
                    placeholder: None,
                    max_visible_chars: 0,
                    key: Some("ti".into()),
                },
                WidgetSpec::Toggle {
                    checked: false,
                    label: "no key".into(),
                    focused: false,
                    key: None,
                },
            ],
            key: None,
        };
        let mut tabbable = Vec::new();
        collect_tabbable(&spec, &mut tabbable);
        // HintBar without a key isn't tabbable; tabbables are
        // Toggle/Button/TextInput/List with non-empty keys.
        assert_eq!(tabbable, vec!["t", "b", "ti"]);
    }

    #[test]
    fn first_render_focuses_first_tabbable() {
        let spec = WidgetSpec::Row {
            children: vec![
                WidgetSpec::Toggle {
                    checked: false,
                    label: "A".into(),
                    focused: false,
                    key: Some("a".into()),
                },
                WidgetSpec::Toggle {
                    checked: false,
                    label: "B".into(),
                    focused: false,
                    key: Some("b".into()),
                },
            ],
            key: None,
        };
        let out = render_spec(&spec, &HashMap::new(), "");
        assert_eq!(out.focus_key, "a");
        assert_eq!(out.tabbable, vec!["a", "b"]);
    }

    #[test]
    fn render_preserves_focus_key_across_re_renders() {
        let spec = WidgetSpec::Row {
            children: vec![
                WidgetSpec::Toggle {
                    checked: false,
                    label: "A".into(),
                    focused: false,
                    key: Some("a".into()),
                },
                WidgetSpec::Toggle {
                    checked: false,
                    label: "B".into(),
                    focused: false,
                    key: Some("b".into()),
                },
            ],
            key: None,
        };
        let out = render_spec(&spec, &HashMap::new(), "b");
        assert_eq!(out.focus_key, "b");
    }

    #[test]
    fn render_clamps_stale_focus_key_to_first_tabbable() {
        // Previous render focused "stale", but the new spec doesn't
        // have any widget with that key — fall back to the first
        // tabbable.
        let spec = WidgetSpec::Toggle {
            checked: false,
            label: "Only".into(),
            focused: false,
            key: Some("only".into()),
        };
        let out = render_spec(&spec, &HashMap::new(), "stale");
        assert_eq!(out.focus_key, "only");
    }

    #[test]
    fn focused_widget_renders_with_focused_styling() {
        let spec = WidgetSpec::Row {
            children: vec![
                WidgetSpec::Toggle {
                    checked: false,
                    label: "A".into(),
                    focused: false,
                    key: Some("a".into()),
                },
                WidgetSpec::Toggle {
                    checked: false,
                    label: "B".into(),
                    focused: false,
                    key: Some("b".into()),
                },
            ],
            key: None,
        };
        let out = render_spec(&spec, &HashMap::new(), "b");
        assert_eq!(out.entries.len(), 1, "row collapses inline");
        // Two overlays expected from the focused B: one for B's
        // glyph (none, since unchecked) — actually unchecked emits
        // no glyph overlay. So only the focused-style overlay.
        // Find the focused overlay by its menu_active_bg key.
        let entry = &out.entries[0];
        let focused_overlay = entry
            .inline_overlays
            .iter()
            .find(|o| {
                o.style
                    .bg
                    .as_ref()
                    .and_then(|c| c.as_theme_key())
                    == Some("ui.menu_active_bg")
            })
            .expect("focused overlay present on B");
        // B's text is "[ ] B", starting after "[ ] A".len()==5 + spacer 0 (no spacer here).
        // Inline collapse: A is "[ ] A" then immediately "[ ] B" = 10 bytes.
        assert_eq!(focused_overlay.start, 5);
        assert_eq!(focused_overlay.end, 10);
    }

    #[test]
    fn no_tabbables_yields_empty_focus_key() {
        let spec = WidgetSpec::Col {
            children: vec![WidgetSpec::HintBar {
                entries: vec![],
                key: None,
            }],
            key: None,
        };
        let out = render_spec(&spec, &HashMap::new(), "");
        assert_eq!(out.focus_key, "");
        assert!(out.tabbable.is_empty());
    }

    // -------------------------------------------------------------
    // List
    // -------------------------------------------------------------

    #[test]
    fn list_emits_one_entry_and_one_hit_per_item() {
        let spec = WidgetSpec::List {
            items: vec![
                TextPropertyEntry::text("alpha"),
                TextPropertyEntry::text("beta"),
                TextPropertyEntry::text("gamma"),
            ],
            item_keys: vec!["a".into(), "b".into(), "c".into()],
            selected_index: -1,
            visible_rows: 10,
            key: None,
        };
        let (entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(entries.len(), 3);
        assert_eq!(hits.len(), 3);
        for (i, h) in hits.iter().enumerate() {
            assert_eq!(h.buffer_row, i as u32);
            assert_eq!(h.widget_kind, "list");
            assert_eq!(h.event_type, "select");
            assert_eq!(h.payload["index"], i);
        }
        assert_eq!(hits[0].widget_key, "a");
        assert_eq!(hits[2].widget_key, "c");
    }

    #[test]
    fn list_applies_selection_bg_to_selected_row() {
        let spec = WidgetSpec::List {
            items: vec![
                TextPropertyEntry::text("first"),
                TextPropertyEntry::text("second"),
            ],
            item_keys: vec!["x".into(), "y".into()],
            selected_index: 1,
            visible_rows: 10,
            key: None,
        };
        let (entries, _hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert!(entries[0].style.is_none(), "unselected row keeps no style");
        let style = entries[1].style.as_ref().expect("selected row gets style");
        assert_eq!(
            style.bg.as_ref().and_then(|c| c.as_theme_key()),
            Some("ui.menu_active_bg"),
        );
        assert!(style.extend_to_line_end);
    }

    #[test]
    fn list_inside_col_offsets_hit_rows_by_preceding_lines() {
        let spec = WidgetSpec::Col {
            children: vec![
                WidgetSpec::HintBar {
                    entries: vec![HintEntry {
                        keys: "h".into(),
                        label: "header".into(),
                    }],
                    key: None,
                },
                WidgetSpec::List {
                    items: vec![
                        TextPropertyEntry::text("row0"),
                        TextPropertyEntry::text("row1"),
                    ],
                    item_keys: vec!["a".into(), "b".into()],
                    selected_index: -1,
                    visible_rows: 10,
                    key: None,
                },
            ],
            key: None,
        };
        let (entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(entries.len(), 3);
        assert_eq!(hits.len(), 2);
        // List rows land at buffer_row 1 and 2 (after the HintBar).
        assert_eq!(hits[0].buffer_row, 1);
        assert_eq!(hits[1].buffer_row, 2);
    }

    #[test]
    fn list_payload_includes_absolute_index_and_key() {
        let spec = WidgetSpec::List {
            items: vec![TextPropertyEntry::text("only")],
            item_keys: vec!["match:42".into()],
            selected_index: 0,
            visible_rows: 10,
            key: None,
        };
        let (_entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(hits[0].payload["index"], 0);
        assert_eq!(hits[0].payload["key"], "match:42");
    }

    #[test]
    fn list_with_missing_key_emits_empty_widget_key() {
        let spec = WidgetSpec::List {
            items: vec![
                TextPropertyEntry::text("a"),
                TextPropertyEntry::text("b"),
            ],
            // Only one key for two items — second hit gets an empty key.
            item_keys: vec!["only".into()],
            selected_index: -1,
            visible_rows: 10,
            key: None,
        };
        let (_, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(hits[0].widget_key, "only");
        assert_eq!(hits[1].widget_key, "");
    }

    fn make_list(selected: i32, visible: u32, total: usize, key: Option<&str>) -> WidgetSpec {
        let items = (0..total)
            .map(|i| TextPropertyEntry::text(format!("row{}", i)))
            .collect();
        let item_keys = (0..total).map(|i| format!("k{}", i)).collect();
        WidgetSpec::List {
            items,
            item_keys,
            selected_index: selected,
            visible_rows: visible,
            key: key.map(|s| s.to_string()),
        }
    }

    #[test]
    fn list_renders_only_visible_window() {
        let spec = make_list(-1, 3, 10, Some("L"));
        let (entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(entries.len(), 3);
        assert_eq!(hits.len(), 3);
        // First three items, absolute indices 0..2.
        assert_eq!(hits[0].payload["index"], 0);
        assert_eq!(hits[2].payload["index"], 2);
    }

    #[test]
    fn list_scrolls_to_keep_selected_below_window_in_view() {
        // 10 items, visible=3, select index 5: scroll should be 3
        // (so selected lands at the bottom of the window). On
        // *first* render (empty prev), the spec's selected_index
        // seeds instance state.
        let spec = make_list(5, 3, 10, Some("L"));
        let (_entries, hits, state) = render_no_focus(&spec, &HashMap::new());
        // Visible window is items 3..6 → hits index 3, 4, 5.
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].payload["index"], 3);
        assert_eq!(hits[2].payload["index"], 5);
        let scroll = match state.get("L").unwrap() {
            WidgetInstanceState::List { scroll_offset, .. } => *scroll_offset,
            _ => unreachable!(),
        };
        assert_eq!(scroll, 3);
    }

    #[test]
    fn list_scrolls_to_keep_selected_above_window_in_view() {
        // Previous render scrolled to 5 with selection at 5; user
        // pressed Up enough times that select_move set instance
        // state's selection to 1; renderer should scroll back up
        // to 1. (Spec's selected_index is initial-only; instance
        // state is authoritative once present.)
        let mut prev = HashMap::new();
        prev.insert(
            "L".into(),
            WidgetInstanceState::List {
                scroll_offset: 5,
                selected_index: 1,
            },
        );
        // Spec's selected_index doesn't matter (instance state wins).
        let spec = make_list(99, 3, 10, Some("L"));
        let (_entries, hits, state) = render_no_focus(&spec, &prev);
        assert_eq!(hits[0].payload["index"], 1);
        let scroll = match state.get("L").unwrap() {
            WidgetInstanceState::List { scroll_offset, .. } => *scroll_offset,
            _ => unreachable!(),
        };
        assert_eq!(scroll, 1);
    }

    #[test]
    fn list_scroll_preserved_when_selection_remains_in_view() {
        // Previous render scrolled to 4 with selection at 4; user
        // moved selection to 5 (still in window 4..6); scroll stays.
        let mut prev = HashMap::new();
        prev.insert(
            "L".into(),
            WidgetInstanceState::List {
                scroll_offset: 4,
                selected_index: 5,
            },
        );
        let spec = make_list(99, 3, 10, Some("L"));
        let (_entries, hits, state) = render_no_focus(&spec, &prev);
        assert_eq!(hits[0].payload["index"], 4);
        let scroll = match state.get("L").unwrap() {
            WidgetInstanceState::List { scroll_offset, .. } => *scroll_offset,
            _ => unreachable!(),
        };
        assert_eq!(scroll, 4);
    }

    #[test]
    fn list_clamps_scroll_to_max_when_dataset_is_smaller_than_old_offset() {
        // Previous scroll past the end of a now-shorter dataset
        // clamps to max_scroll = total - visible.
        let mut prev = HashMap::new();
        prev.insert("L".into(), WidgetInstanceState::List { scroll_offset: 8, selected_index: -1 });
        let spec = make_list(-1, 3, 5, Some("L"));
        let (entries, _hits, state) = render_no_focus(&spec, &prev);
        assert_eq!(entries.len(), 3);
        let scroll = match state.get("L").unwrap() {
            WidgetInstanceState::List { scroll_offset, .. } => *scroll_offset,
            _ => unreachable!(),
        };
        // total=5, visible=3 → max=2.
        assert_eq!(scroll, 2);
    }

    #[test]
    fn list_does_not_scroll_when_total_smaller_than_visible() {
        let spec = make_list(-1, 10, 3, Some("L"));
        let (entries, _hits, state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(entries.len(), 3, "all items fit");
        let scroll = match state.get("L").unwrap() {
            WidgetInstanceState::List { scroll_offset, .. } => *scroll_offset,
            _ => unreachable!(),
        };
        assert_eq!(scroll, 0);
    }

    #[test]
    fn list_without_key_does_not_persist_state() {
        let spec = make_list(5, 3, 10, None);
        let (_entries, _hits, state) = render_no_focus(&spec, &HashMap::new());
        assert!(
            state.is_empty(),
            "Lists without a `key` opt out of state preservation"
        );
    }

    // -------------------------------------------------------------
    // TextInput
    // -------------------------------------------------------------

    #[test]
    fn text_input_renders_value_in_brackets() {
        let entry = render_text_input("hello", -1, false, "", None, 0);
        assert_eq!(entry.text, "[hello]");
        assert!(entry.inline_overlays.is_empty());
    }

    #[test]
    fn text_input_with_label_prefixes_with_label_space() {
        let entry = render_text_input("foo", -1, false, "Search:", None, 0);
        assert_eq!(entry.text, "Search: [foo]");
    }

    #[test]
    fn text_input_focused_adds_input_bg_overlay() {
        let entry = render_text_input("x", -1, true, "", None, 0);
        // Focused → input-bg overlay (no cursor since cursor_byte < 0).
        assert_eq!(entry.inline_overlays.len(), 1);
        let bg = entry.inline_overlays[0].style.bg.as_ref().unwrap();
        assert_eq!(bg.as_theme_key(), Some("ui.prompt_bg"));
    }

    #[test]
    fn text_input_cursor_at_value_position_highlights_char() {
        let entry = render_text_input("abc", 1, true, "", None, 0);
        // Two overlays: input-bg (focused) + cursor on 'b'.
        assert_eq!(entry.inline_overlays.len(), 2);
        let cursor = entry
            .inline_overlays
            .iter()
            .find(|o| {
                o.style
                    .bg
                    .as_ref()
                    .map(|c| c.as_theme_key() == Some("editor.cursor"))
                    .unwrap_or(false)
            })
            .expect("cursor overlay present");
        // 'a' is at byte 1 (after '['), 'b' at byte 2, 'c' at byte 3
        // when label="" (text = "[abc]").
        assert_eq!(cursor.start, 2);
        assert_eq!(cursor.end, 3);
    }

    #[test]
    fn text_input_cursor_at_end_highlights_closing_bracket() {
        let entry = render_text_input("ab", 2, true, "", None, 0);
        let cursor = entry
            .inline_overlays
            .iter()
            .find(|o| {
                o.style
                    .bg
                    .as_ref()
                    .map(|c| c.as_theme_key() == Some("editor.cursor"))
                    .unwrap_or(false)
            })
            .unwrap();
        // text = "[ab]" → closing bracket at byte 3..4
        assert_eq!(cursor.start, 3);
        assert_eq!(cursor.end, 4);
    }

    #[test]
    fn text_input_unfocused_empty_shows_placeholder_in_muted() {
        let entry = render_text_input("", -1, false, "", Some("type here"), 0);
        assert_eq!(entry.text, "[type here]");
        // One overlay for the placeholder muted color.
        assert_eq!(entry.inline_overlays.len(), 1);
        let fg = entry.inline_overlays[0].style.fg.as_ref().unwrap();
        assert_eq!(fg.as_theme_key(), Some("ui.menu_disabled_fg"));
    }

    #[test]
    fn text_input_focused_empty_does_not_show_placeholder() {
        let entry = render_text_input("", -1, true, "", Some("type here"), 0);
        // No placeholder when focused — would obstruct the cursor.
        assert_eq!(entry.text, "[]");
    }

    #[test]
    fn text_input_truncates_long_value_keeping_tail_visible() {
        let value: String = "0123456789abcdefghij".to_string();
        let entry = render_text_input(&value, -1, false, "", None, 6);
        // Tail-truncated to "…fghij" (max=6, take=5 chars).
        assert_eq!(entry.text, "[…fghij]");
    }

    #[test]
    fn raw_inside_col_offsets_following_hits() {
        let spec = WidgetSpec::Col {
            children: vec![
                WidgetSpec::Raw {
                    entries: vec![
                        TextPropertyEntry::text("line0"),
                        TextPropertyEntry::text("line1"),
                        TextPropertyEntry::text("line2"),
                    ],
                    key: None,
                },
                WidgetSpec::Toggle {
                    checked: false,
                    label: "after raw".into(),
                    focused: false,
                    key: Some("post".into()),
                },
            ],
            key: None,
        };
        let (entries, hits, _state) = render_no_focus(&spec, &HashMap::new());
        assert_eq!(entries.len(), 4);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].buffer_row, 3);
    }
}
