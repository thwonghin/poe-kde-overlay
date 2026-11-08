# apt-kwin-overlay: code-quality follow-ups

Open items only; git history records what was fixed.

## Known simplifications (`ponytail:` notes in the code)

Deliberate, each with a stated ceiling; listed so they're not forgotten.

- **Alt-hold hide only un-hides on modifier changes**
  (`overlay_visibility.rs`): upstream also un-hides on any key or mouse
  move while Alt is held; we only see modifier transitions.
- **No mousedown-in-area handler** (`shortcuts.rs`, `spawn_auto_close`): a
  click inside the popup is covered by the move that precedes it.
