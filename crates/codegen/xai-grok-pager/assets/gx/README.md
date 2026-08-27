# gx welcome-screen owl

Braille art for the gx splash (and the compact minimal-mode card). A stamped gx
build (`GROK_VERSION` contains `+gx.`) shows this instead of upstream's Grok `g`.
Stock `grok` is unchanged.

| File | Size | Used when |
|---|---|---|
| `owl07.txt` | 7 rows × 17 cols | hero box + tall terminals (`h >= 26`) |
| `owl05.txt` | 5 rows × 12 cols | short terminals + minimal welcome card |
| `owl_logo.svg` | silhouette snapshot | regenerating the `.txt` files |

Row counts match upstream `assets/logo/logo07.txt` / `logo05.txt` on purpose:
welcome layout math and tests key off height, not width. Native aspect makes the
owl a few columns wider than the Grok mark.

`owl_logo.svg` is a snapshot of the StrideLabs owl silhouette (same mark roost,
lumen, and tapper ship). Do not edit `../roost` from this fork; if the mark
changes, copy a new snapshot here and re-rasterize.

The TUI embeds the `.txt` files via `include_str!` in
`src/views/welcome/logo.rs`. Empty cells are U+2800 (braille blank), matching
upstream.
