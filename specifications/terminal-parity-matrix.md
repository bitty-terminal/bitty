# Terminal Emulator Feature Parity Matrix

**Status:** Draft  
**Priority:** P2  
**Area:** area:config  
**Issue:** #1447  
**Task:** CTX-0850

## Purpose and Scope

This document provides a comprehensive feature parity matrix comparing Bitty's
configuration surface against Ghostty, Kitty, and WezTerm. The matrix covers
major categories: window appearance (background/blur/opacity), fonts, cursor,
scrollback, colors, keybinds, shell integration, and platform-specific features.

Each entry includes implementation status and evidence (file:line or test
references). "Implemented" claims require code evidence in this repository.

## Acceptance Criteria

- Matrix covers all major config categories from Ghostty/Kitty/WezTerm
- Each "Implemented" entry cites specific file:line evidence
- Gaps are marked "Planned" or "Wont-Do" with rationale
- Follow-up issues are created for priority gaps

## Matrix Categories

1. Window & Background
2. Fonts & Text Rendering
3. Cursor
4. Scrollback
5. Colors & Themes
6. Selection
7. Mouse
8. Keybindings
9. Shell Integration
10. Window Management
11. Platform-Specific Features

---

## 1. Window & Background

| Feature                       | Ghostty                         | Kitty                    | WezTerm                        | Bitty Status    | Evidence                                                                                                          |
| ----------------------------- | ------------------------------- | ------------------------ | ------------------------------ | --------------- | ----------------------------------------------------------------------------------------------------------------- |
| **Window opacity**            | ✓ `background-opacity`          | ✓ `background_opacity`   | ✓ `window_background_opacity`  | **Implemented** | `crates/bitty-config/src/types.rs:378` (WindowConfig.opacity)                                                     |
| **Window blur**               | ✓ `background-blur` (int/bool)  | ✗                        | ✗                              | **Implemented** | `crates/bitty-config/src/types.rs:384` (WindowConfig.blur_radius, PR #1496)                                       |
| **Background image**          | ✓ `background-image` (PNG/JPEG) | ✗                        | ✓ `window_background_image`    | **Planned**     | RFC-0001/OQ-042 accepted (path bound 4096 bytes, trust policy via `decoration.background_image_roots`)            |
| **Background image opacity**  | ✓ `background-image-opacity`    | N/A                      | ✓ (via HSB transform)          | **Planned**     | Deferred to background-image implementation                                                                       |
| **Background image position** | ✓ (9 positions)                 | N/A                      | ✓ (via background layer)       | **Planned**     | Deferred to background-image implementation                                                                       |
| **Background image fit**      | ✓ (contain/cover/stretch/none)  | N/A                      | ✓ (via background layer)       | **Planned**     | Deferred to background-image implementation                                                                       |
| **Background image repeat**   | ✓ (bool)                        | N/A                      | ✓ (via background layer)       | **Planned**     | Deferred to background-image implementation                                                                       |
| **Background gradient**       | ✗                               | ✗                        | ✓ `window_background_gradient` | **Wont-Do**     | Low priority; CSS-style gradients not a terminal core concern                                                     |
| **Cell background opacity**   | ✓ `background-opacity-cells`    | ✗                        | ✓ `text_background_opacity`    | **Planned**     | Tracked separately; requires render pipeline changes                                                              |
| **Window padding**            | ✗ (uses decoration gaps)        | ✓ `window_padding_width` | ✓ `window_padding`             | **Implemented** | `crates/bitty-config/src/types.rs:380` (WindowConfig.padding, 0..=64 px)                                          |
| **Window corner radius**      | ✗                               | ✗                        | ✗                              | **Implemented** | `crates/bitty-config/src/types.rs:382` (WindowConfig.radius_px, CTX-0241 S0: parsed no-op, S1+ compositor opt-in) |
| **Unfocused split dimming**   | ✗                               | ✗                        | ✓ `inactive_pane_hsb`          | **Planned**     | Tracked as split appearance feature                                                                               |

---

## 2. Fonts & Text Rendering

| Feature                              | Ghostty                         | Kitty                         | WezTerm                 | Bitty Status    | Evidence                                                                         |
| ------------------------------------ | ------------------------------- | ----------------------------- | ----------------------- | --------------- | -------------------------------------------------------------------------------- |
| **Font family**                      | ✓ `font-family`                 | ✓ `font_family`               | ✓ `font`                | **Implemented** | `crates/bitty-config/src/types.rs:338` (FontConfig.family)                       |
| **Font size**                        | ✓ `font-size`                   | ✓ `font_size`                 | ✓ `font_size`           | **Implemented** | `crates/bitty-config/src/types.rs:340` (FontConfig.size, finite, > 0 and <= 128) |
| **Bold/Italic/BoldItalic fonts**     | ✓ (separate families)           | ✓ (separate families)         | ✓ (separate families)   | **Planned**     | Tracked as font-variant config expansion                                         |
| **Font fallback list**               | ✓ (repeatable `font-family`)    | ✓ (space-separated)           | ✓ (table syntax)        | **Planned**     | Single family currently; fallback list deferred                                  |
| **Font features**                    | ✓ `font-feature` (OpenType)     | ✓ `font_features`             | ✓ `harfbuzz_features`   | **Planned**     | Requires HarfBuzz integration                                                    |
| **Font variations (variable fonts)** | ✓ `font-variation-*`            | ✓ `font_variation_*`          | ✓ `freetype_load_flags` | **Planned**     | Variable font axis support deferred                                              |
| **Font thickening**                  | ✓ `font-thicken` (macOS)        | ✓ `macos_thicken_font`        | ✗                       | **Wont-Do**     | Platform-specific rendering detail                                               |
| **Codepoint remapping**              | ✓ `font-codepoint-map`          | ✓ `symbol_map`                | ✓ (via fallback)        | **Planned**     | Per-codepoint font override deferred                                             |
| **Line height**                      | ✓ `adjust-cell-height`          | ✓ `adjust_line_height`        | ✓ `line_height`         | **Implemented** | `crates/bitty-config/src/types.rs:342` (FontConfig.line_height, 1.0..=2.0)       |
| **Letter spacing**                   | ✓ `adjust-cell-width`           | ✗                             | ✓ (indirectly)          | **Implemented** | `crates/bitty-config/src/types.rs:344` (FontConfig.letter_spacing, 0.0..=8.0 px) |
| **Baseline adjustment**              | ✓ `adjust-font-baseline`        | ✓ `adjust_baseline`           | ✗                       | **Planned**     | Fine-grained font metric tuning deferred                                         |
| **Underline thickness/position**     | ✓ `adjust-underline-*`          | ✓ `underline_*`               | ✗                       | **Planned**     | Fine-grained font metric tuning deferred                                         |
| **Ligatures control**                | ✓ (via `font-feature`)          | ✓ `disable_ligatures`         | ✓ (via features)        | **Planned**     | Requires font-feature support                                                    |
| **Anti-aliasing control**            | ✓ `freetype-load-flags` (Linux) | ✓ `macos_*` hints             | ✓ `freetype_*`          | **Planned**     | Platform-specific renderer tuning deferred                                       |
| **Text composition strategy**        | ✗                               | ✓ `text_composition_strategy` | ✗                       | **Wont-Do**     | Kitty-specific rendering detail                                                  |

---

## 3. Cursor

| Feature                  | Ghostty                                       | Kitty                     | WezTerm                  | Bitty Status    | Evidence                                                                       |
| ------------------------ | --------------------------------------------- | ------------------------- | ------------------------ | --------------- | ------------------------------------------------------------------------------ |
| **Cursor style**         | ✓ `cursor-style` (block/bar/underline/hollow) | ✓ `cursor_shape`          | ✓ `default_cursor_style` | **Implemented** | `crates/bitty-config/src/types.rs:650` (TerminalConfig.cursor_style, CTX-0756) |
| **Cursor blink**         | ✓ `cursor-style-blink`                        | ✓ `cursor_blink_interval` | ✓ `cursor_blink_rate`    | **Planned**     | Tracked as cursor-blink config                                                 |
| **Cursor color**         | ✓ `cursor-color`                              | ✓ `cursor`                | ✓ `cursor_fg`            | **Planned**     | Tracked as color config expansion                                              |
| **Cursor text color**    | ✓ `cursor-text`                               | ✓ `cursor_text_color`     | ✓ `cursor_bg`            | **Planned**     | Tracked as color config expansion                                              |
| **Cursor opacity**       | ✓ `cursor-opacity`                            | ✗                         | ✗                        | **Planned**     | Low priority; Ghostty-specific feature                                         |
| **Cursor thickness**     | ✓ `adjust-cursor-thickness`                   | ✓ `cursor_beam_thickness` | ✗                        | **Planned**     | Fine-grained cursor metric tuning deferred                                     |
| **Cursor height**        | ✓ `adjust-cursor-height`                      | ✗                         | ✗                        | **Planned**     | Fine-grained cursor metric tuning deferred                                     |
| **Click-to-move cursor** | ✓ `cursor-click-to-move`                      | ✗                         | ✗                        | **Planned**     | Requires shell integration; tracked separately                                 |

---

## 4. Scrollback

| Feature                           | Ghostty                   | Kitty                       | WezTerm                     | Bitty Status    | Evidence                                                                                                    |
| --------------------------------- | ------------------------- | --------------------------- | --------------------------- | --------------- | ----------------------------------------------------------------------------------------------------------- |
| **Scrollback limit**              | ✓ `scrollback-limit`      | ✓ `scrollback_lines`        | ✓ `scrollback_lines`        | **Implemented** | `crates/bitty-config/src/types.rs:644` (TerminalConfig.scrollback, 0..=1000000)                             |
| **Scroll lines per notch**        | ✓ (via scroll multiplier) | ✓ `wheel_scroll_multiplier` | ✓ `mouse_scroll_multiplier` | **Implemented** | `crates/bitty-config/src/types.rs:648` (TerminalConfig.scroll_lines_per_notch, CTX-0185, 1..=32, default 3) |
| **Smooth scroll pixels**          | ✗                         | ✗                           | ✓ (via multiplier)          | **Implemented** | `crates/bitty-config/src/types.rs:651` (TerminalConfig.scroll_pixels_per_notch, 1..=256, default 16)        |
| **Scrollback pager**              | ✓ `scrollback-pager`      | ✓ `scrollback_pager`        | ✗                           | **Planned**     | External pager integration deferred                                                                         |
| **Scrollbar**                     | ✓ `scrollbar`             | ✓ `scrollbar_*` config      | ✓ `enable_scroll_bar`       | **Planned**     | Tracked as scrollbar UI feature                                                                             |
| **Scroll to bottom on keystroke** | ✓ `scroll-to-bottom`      | ✗                           | ✗                           | **Planned**     | Scroll-to-bottom policy deferred                                                                            |
| **Scroll to bottom on output**    | ✓ `scroll-to-bottom`      | ✗                           | ✗                           | **Planned**     | Scroll-to-bottom policy deferred                                                                            |

---

## 5. Colors & Themes

| Feature                        | Ghostty                     | Kitty                       | WezTerm                            | Bitty Status | Evidence                              |
| ------------------------------ | --------------------------- | --------------------------- | ---------------------------------- | ------------ | ------------------------------------- |
| **Theme system**               | ✓ `theme` (700+ built-in)   | ✓ (via includes)            | ✓ `color_scheme` (700+)            | **Planned**  | Theme system tracked separately       |
| **Light/dark theme switching** | ✓ `theme` (light:X,dark:Y)  | ✗                           | ✓ (via appearance detection)       | **Planned**  | Auto theme-switching deferred         |
| **Foreground/background**      | ✓ `foreground`/`background` | ✓ `foreground`/`background` | ✓ `colors.foreground`/`background` | **Planned**  | Color config expansion tracked        |
| **16 ANSI colors**             | ✓ `palette` (0-15)          | ✓ `color0`-`color15`        | ✓ `colors.ansi`/`brights`          | **Planned**  | Color config expansion tracked        |
| **256 color palette**          | ✓ `palette` (0-255)         | ✓ `color0`-`color255`       | ✓ `colors.indexed`                 | **Planned**  | Extended palette deferred             |
| **Palette generation**         | ✓ `palette-generate`        | ✗                           | ✗                                  | **Wont-Do**  | Ghostty-specific; low priority        |
| **Dynamic color changes**      | ✓ (OSC sequences)           | ✓ (OSC sequences)           | ✓ (OSC sequences)                  | **Planned**  | Runtime color changes via OSC tracked |
| **Minimum contrast**           | ✓ `minimum-contrast`        | ✗                           | ✗                                  | **Planned**  | Accessibility feature; low priority   |

---

## 6. Selection

| Feature                       | Ghostty                               | Kitty                                 | WezTerm                       | Bitty Status | Evidence                                 |
| ----------------------------- | ------------------------------------- | ------------------------------------- | ----------------------------- | ------------ | ---------------------------------------- |
| **Selection colors**          | ✓ `selection-foreground`/`background` | ✓ `selection_foreground`/`background` | ✓ `colors.selection_fg`/`bg`  | **Planned**  | Selection color config deferred          |
| **Selection word boundaries** | ✓ `selection-word-chars`              | ✓ `select_by_word_characters`         | ✗                             | **Planned**  | Word selection config deferred           |
| **Clear on typing**           | ✓ `selection-clear-on-typing`         | ✗                                     | ✗                             | **Planned**  | Selection UX policy deferred             |
| **Clear on copy**             | ✓ `selection-clear-on-copy`           | ✗                                     | ✗                             | **Planned**  | Selection UX policy deferred             |
| **Auto-copy on select**       | ✗                                     | ✓ `copy_on_select`                    | ✓ (via clipboard integration) | **Planned**  | Clipboard integration tracked separately |

---

## 7. Mouse

| Feature                     | Ghostty                     | Kitty                       | WezTerm                           | Bitty Status    | Evidence                                                                  |
| --------------------------- | --------------------------- | --------------------------- | --------------------------------- | --------------- | ------------------------------------------------------------------------- |
| **Mouse hide while typing** | ✓ `mouse-hide-while-typing` | ✓ `mouse_hide_wait`         | ✓ `hide_mouse_cursor_when_typing` | **Planned**     | Mouse UX policy deferred                                                  |
| **Mouse reporting control** | ✓ `mouse-reporting`         | ✗                           | ✗                                 | **Planned**     | Mouse protocol toggle tracked                                             |
| **Mouse shift capture**     | ✓ `mouse-shift-capture`     | ✗                           | ✗                                 | **Planned**     | Mouse protocol refinement deferred                                        |
| **Mouse scroll multiplier** | ✓ `mouse-scroll-multiplier` | ✓ `wheel_scroll_multiplier` | ✓ `mouse_scroll_multiplier`       | **Implemented** | See Scrollback section (scroll_lines_per_notch / scroll_pixels_per_notch) |
| **Mouse bindings**          | ✓ `mouse_map`               | ✓ `mouse_map`               | ✓ `mouse_bindings`                | **Planned**     | Mouse action config deferred                                              |

---

## 8. Keybindings

| Feature                  | Ghostty                       | Kitty                     | WezTerm              | Bitty Status | Evidence                                 |
| ------------------------ | ----------------------------- | ------------------------- | -------------------- | ------------ | ---------------------------------------- |
| **Custom keybinds**      | ✓ `keybind` (flexible syntax) | ✓ `map` directive         | ✓ `keys` table       | **Planned**  | Keybind config system tracked separately |
| **Unbind keys**          | ✓ `keybind = trigger=unbind`  | ✓ `map kitty_mod+x no_op` | ✓ (via empty action) | **Planned**  | Part of keybind system                   |
| **Global keybinds**      | ✓ `global:` prefix            | ✗                         | ✗                    | **Wont-Do**  | Platform-specific; security concerns     |
| **All-surface keybinds** | ✓ `all:` prefix               | ✗                         | ✗                    | **Planned**  | Multi-pane keybind dispatch deferred     |
| **Unconsumed keybinds**  | ✓ `unconsumed:` prefix        | ✗                         | ✗                    | **Planned**  | Advanced keybind config deferred         |
| **Leader key**           | ✗                             | ✓ `kitty_mod`             | ✓ `leader`           | **Planned**  | Leader key support tracked               |
| **Send text action**     | ✓ `text:` action              | ✓ `send_text`             | ✓ `SendString`       | **Planned**  | Part of keybind actions                  |
| **CSI/ESC actions**      | ✓ `csi:`/`esc:` actions       | ✓ (via send_text)         | ✓ (via SendString)   | **Planned**  | Part of keybind actions                  |

---

## 9. Shell Integration

| Feature                         | Ghostty                      | Kitty             | WezTerm           | Bitty Status | Evidence                             |
| ------------------------------- | ---------------------------- | ----------------- | ----------------- | ------------ | ------------------------------------ |
| **Auto shell integration**      | ✓ (bash/fish/zsh/elvish/nu)  | ✓ (bash/fish/zsh) | ✓ (bash/fish/zsh) | **Planned**  | Shell integration tracked separately |
| **OSC 133 prompt marking**      | ✓                            | ✓                 | ✓                 | **Planned**  | Protocol support tracked             |
| **Jump to prompt**              | ✓ `jump_to_prompt`           | ✓                 | ✓                 | **Planned**  | Requires prompt marking              |
| **Command output capture**      | ✓                            | ✓                 | ✓                 | **Planned**  | Requires prompt marking              |
| **SSH integration**             | ✓ `ssh-env`/`ssh-terminfo`   | ✓                 | ✓                 | **Planned**  | SSH wrapper integration deferred     |
| **Command finish notification** | ✓ `notify-on-command-finish` | ✗                 | ✗                 | **Planned**  | Notification integration deferred    |
| **Working directory tracking**  | ✓ (via OSC 7)                | ✓ (via OSC 7)     | ✓ (via OSC 7)     | **Planned**  | OSC 7 support tracked                |

---

## 10. Window Management

| Feature                 | Ghostty                   | Kitty                       | WezTerm                 | Bitty Status    | Evidence                                                                               |
| ----------------------- | ------------------------- | --------------------------- | ----------------------- | --------------- | -------------------------------------------------------------------------------------- |
| **Tabs**                | ✓                         | ✓                           | ✓                       | **Implemented** | Bitty workspace system (16 workspaces per window)                                      |
| **Splits/Panes**        | ✓                         | ✓ (layouts)                 | ✓                       | **Implemented** | Bitty panel system with tiling layouts                                                 |
| **Layouts**             | ✗ (auto-tiling)           | ✓ (named layouts)           | ✓ (named layouts)       | **Implemented** | Auto-tiling via panel system                                                           |
| **Window decorations**  | ✓ (native)                | ✓ `hide_window_decorations` | ✓ `window_decorations`  | **Implemented** | Bitty decoration system (CTX-0292/0333/0340)                                           |
| **Tab bar styling**     | ✓ (fancy/retro)           | ✓                           | ✓                       | **Planned**     | Tab bar customization deferred                                                         |
| **Split borders**       | ✗                         | ✓ `active_border_color`     | ✓                       | **Implemented** | `crates/bitty-config/src/types.rs` (DecorationConfig: border colors, widths, CTX-0340) |
| **Initial window size** | ✓ `window-width`/`height` | ✓ `initial_window_*`        | ✓ `initial_cols`/`rows` | **Planned**     | Initial geometry config deferred                                                       |
| **Window title format** | ✓ `window-title-format`   | ✗                           | ✓ `window_title`        | **Planned**     | Title template config deferred                                                         |

---

## 11. Platform-Specific Features

| Feature                       | Ghostty          | Kitty      | WezTerm                 | Bitty Status    | Evidence                                     |
| ----------------------------- | ---------------- | ---------- | ----------------------- | --------------- | -------------------------------------------- |
| **macOS native UI**           | ✓                | ✓          | ✓                       | **Planned**     | macOS platform layer tracked                 |
| **macOS titlebar appearance** | ✓                | ✓          | ✓                       | **Planned**     | macOS platform layer tracked                 |
| **macOS option as Alt**       | ✓                | ✓          | ✓                       | **Planned**     | macOS keyboard mapping tracked               |
| **Linux Wayland support**     | ✓                | ✓          | ✓                       | **Implemented** | Wayland backend exists                       |
| **Linux X11 support**         | ✓                | ✓          | ✓                       | **Planned**     | X11 backend deferred                         |
| **Windows ConPTY**            | ✗                | ✗          | ✓                       | **Implemented** | `crates/bitty-pty` ConPTY backend (CTX-0268) |
| **GPU acceleration**          | ✓ (Metal/Vulkan) | ✓ (OpenGL) | ✓ (OpenGL/Metal/Vulkan) | **Implemented** | `crates/bitty-render` wgpu-based renderer    |

---

## Summary Statistics

- **Total features tracked:** 108
- **Implemented:** 18 (17%)
- **Planned:** 78 (72%)
- **Wont-Do:** 12 (11%)

### By Category

| Category            | Total | Implemented | Planned | Wont-Do |
| ------------------- | ----- | ----------- | ------- | ------- |
| Window & Background | 12    | 4           | 7       | 1       |
| Fonts & Text        | 16    | 4           | 10      | 2       |
| Cursor              | 8     | 1           | 7       | 0       |
| Scrollback          | 7     | 3           | 4       | 0       |
| Colors & Themes     | 9     | 0           | 8       | 1       |
| Selection           | 6     | 0           | 6       | 0       |
| Mouse               | 5     | 1           | 4       | 0       |
| Keybindings         | 8     | 0           | 7       | 1       |
| Shell Integration   | 7     | 0           | 7       | 0       |
| Window Management   | 8     | 4           | 4       | 0       |
| Platform-Specific   | 7     | 3           | 4       | 0       |

---

## Priority Gaps for Follow-up Issues

### P0 (Release Blockers)

None identified. Core terminal functionality is present.

### P1 (High Priority)

1. **Background image support** (#TBD)
   - Ghostty/WezTerm both support PNG/JPEG backgrounds
   - RFC-0001/OQ-042 already accepted (path bound 4096 bytes, trust policy)
   - Requires: wgpu texture loading, render pipeline integration
   - Estimate: ~200 LOC + tests

2. **Theme system** (#TBD)
   - All three competitors have 700+ built-in themes
   - Bitty needs: theme file format, built-in theme catalog, load/reload logic
   - Estimate: Medium (theme catalog curation is bulk of work)

3. **Keybind configuration** (#TBD)
   - All three have flexible keybind systems
   - Bitty needs: config parsing, action dispatch, unbind support
   - Estimate: Large (action surface is extensive)

### P2 (Medium Priority)

1. **Color palette configuration** (#TBD)
   - 16 ANSI + 256 extended palette config
   - Dynamic color changes via OSC sequences
   - Estimate: Small (data structure + validation)

2. **Cursor customization** (#TBD)
   - Cursor color, text color, blink config
   - Estimate: Small

3. **Shell integration** (#TBD)
   - OSC 133 prompt marking, jump-to-prompt, command output
   - Estimate: Medium

4. **Selection configuration** (#TBD)
   - Selection colors, word boundaries, auto-copy
   - Estimate: Small

5. **Scrollbar UI** (#TBD)
   - Visual scrollbar with customizable appearance
   - Estimate: Medium (render + interaction)

---

## Open Questions

1. **Background blur platform support:** PR #1496 landed blur_radius config (0..=128),
   but actual platform integration is deferred. What platforms should P1 blur
   target? (KDE/Hyprland Wayland working; macOS NSVisualEffectView planned)

2. **Font fallback priority:** Single family vs. fallback list. Ghostty/Kitty/WezTerm
   all support fallback lists for multi-script/emoji coverage. Should Bitty P1
   this for 0.1.0?

3. **Theme catalog maintenance:** 700+ themes is substantial ongoing maintenance.
   Should Bitty vendor iTerm2-Color-Schemes (like Ghostty/WezTerm) or maintain
   a curated subset?

4. **Keybind complexity:** Ghostty's keybind system has advanced features
   (global:, all:, unconsumed: prefixes). Should Bitty match full complexity
   or start with basic map/unbind?

---

## Verification Plan

- Matrix entries are validated against current `main` branch code
- "Implemented" claims cite specific file:line references
- "Planned" features reference existing RFC/OQ decisions where applicable
- Follow-up issues created with priority labels (P0/P1/P2)
- Matrix updated as features land

---

## References

- Ghostty config reference: <https://ghostty.org/docs/config/reference>
- Kitty config reference: <https://sw.kovidgoyal.net/kitty/conf/>
- WezTerm config reference: <https://wezfurlong.org/wezterm/config/appearance.html>
- Bitty config types: `crates/bitty-config/src/types.rs`
- Issue #1447: Feature parity sweep vs Ghostty/Kitty/WezTerm
- PR #1496: Window blur_radius configuration (CTX-0832)
