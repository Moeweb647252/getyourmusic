# Vendored: mediaremote-adapter

- Upstream: https://github.com/ungive/mediaremote-adapter
- Commit: 29718252613a5b0e210bdc64de0bd944ab379706 (2026-09-30)
- License: BSD 3-Clause (see `LICENSE`)

Only the sources needed to build `MediaRemoteAdapter.framework` and the
`bin/mediaremote-adapter.pl` loader are included. The framework is built by
`crates/gym-platform/build.rs` and loaded at runtime by the system Perl
(`/usr/bin/perl`), which is entitled to use the private MediaRemote framework.
