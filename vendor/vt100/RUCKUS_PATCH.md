# Vendored + patched vt100 (0.15.2)

Forked because upstream vt100 discards **OSC 8 hyperlinks** (only OSC 0/1/2 are
handled). ruckus needs the URL to make links like Claude's clickable.

Patch (search `ruckus`):
- `cell.rs`: `Cell` gains an `Option<Arc<str>>` hyperlink + `hyperlink()` accessor;
  `clear()` resets it. `Arc` keeps `Screen`/`Parser` `Send + Sync`.
- `screen.rs`: `Screen.current_hyperlink` set/cleared by `OSC 8 ; params ; URI`;
  stamped onto each cell as it's drawn.

Upstream is MIT-licensed (see LICENSE). Re-apply on any vt100 bump.
