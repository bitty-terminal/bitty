# Issue #1445 Implementation Notes

## Analysis

### Problem 1: Corner-Drag Resize

Currently, `hit_test_split_handle` returns a single split path. For corner-drag to work with 4 panels, we need to detect when the cursor is at the intersection of two perpendicular splits (one horizontal, one vertical) and resize both simultaneously.

### Problem 2: Direction Mapping

The issue states "Ctrl+Shift+hjkl vs Ctrl+Shift+Mod+hjkl resize directions are swapped".

Looking at the code:

- Both binding sets map to the same actions (resize_split:left/down/up/right)
- The implementation in `resize_focused_pane` treats Right/Down as "positive" directions
- When focus is in first child and direction is positive, delta is +0.1 (grow)
- When focus is in first child and direction is negative (Left/Up), delta is -0.1 (shrink)

The bug is likely that the hjkl mapping itself is incorrect, or the comment in the issue indicates the **behavior** is swapped between the two binding variants (though they both call the same function, so this seems unlikely unless there's mod-aware logic I'm missing).

Upon re-reading: "hjkl = left/down/up/right" - this is the VIM standard and matches current bindings.

The issue says directions are "swapped" - this most likely means the resize behavior is opposite to user expectation. For example:

- User presses Shift+Ctrl+h (resize:left)
- Expects: focused pane grows to the left
- Actual: focused pane shrinks from the left (or grows right)

Let me check if the issue is that the logic should be inverted.

## Implementation Plan

### Part 1: Corner-Drag (Multi-Split Resize)

1. Add `hit_test_split_handles` (plural) that returns `Vec<(Vec<usize>, SplitAxis)>`
2. Modify `BorderDragState` to hold multiple split paths
3. Update `update_border_drag` to apply appropriate deltas to all splits
4. Add comprehensive tests

### Part 2: Direction Fix

Need to verify expected behavior and fix if inverted. The safest fix is to add regression tests that pin the expected behavior based on user feedback from the issue.
