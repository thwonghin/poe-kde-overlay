# WIP: PoE2 (Exiled Exchange 2) gaps

PoE2 runs through the EE2 renderer as a peer of PoE1 (`bb10c52`, CI in
`bf410c9`). Price-check works. These are what's still missing or assumed.

## Gaps

1. **poe2's "Filter by" option is hidden.** EE2's Item info tab
   (`settings-item-check.vue`) puts search-same-priced's Price Only /
   Price and Item select on the same row as its hotkey field.
   `src/hotkey_ui_info.js` hides that whole tab behind the KDE banner
   (it assumes the tab is hotkey rows only, true for poe1), so the
   setting can't be changed.

## Known limits (by design)

- A hotkey pressed during the 1–2 s WebView reload after switching games
  is lost (`ponytail:` in `Overlay::switch_game`).
