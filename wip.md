# WIP: PoE2 (Exiled Exchange 2) gaps

PoE2 runs through the EE2 renderer as a peer of PoE1 (`bb10c52`, CI in
`bf410c9`). Price-check works. Nothing known missing; see the limits below.

## Known limits (by design)

- A hotkey pressed during the 1–2 s WebView reload after switching games
  is lost (`ponytail:` in `Overlay::switch_game`).
