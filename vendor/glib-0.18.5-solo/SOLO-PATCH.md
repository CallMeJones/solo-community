# Solo glib 0.18 soundness backport

This directory starts from the published `glib` 0.18.5 crate. The crate's
registry checksum is
`233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5`.
It retains the upstream MIT license and copyright notices.

Solo applies the two-token soundness fix for
`RUSTSEC-2024-0429` / `GHSA-wrw7-89jp-8q8g`: the out-pointer passed to
`g_variant_get_child` is mutable and is passed as `&mut p`. The change is
byte-for-byte equivalent to upstream's reviewed, signed commit
`b5a4071e439bef2b5eea76c3aa25e5ae84839e34`.

The local patch is necessary because the GTK3 ecosystem requires `glib
^0.18`, while gtk-rs has declared the 0.18 line end-of-life and will not cut a
fixed 0.18.x release. Version-only scanners still classify this source as
affected, so CI ignores this one advisory only after
`scripts/verify_patched_glib.sh` proves that Cargo resolves the vendored crate
and that the patched source has not drifted. Any other Rust vulnerability or
unsoundness finding remains fatal.

Remove this directory, the root `[patch.crates-io]` entry, the verifier, and
the single audit exception together when Solo's tray stack no longer depends
on GTK3/glib 0.18.
