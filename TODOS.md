# TODOs

- [ ] Add explicit Swarm volume import/adoption support, with node placement, attached-consumer checks, data preservation and operator confirmation. Existing `--adopt-existing-volumes` is Compose-only; Swarm setup does not import volume data.
- [ ] Make setup's next-command hint backend-aware: `dks up` for Compose, `dks deploy` for Swarm, while keeping JSON output stable.
- [ ] Include source revision in version/diagnostic output so unreleased binaries can be identified without confusing them with a tagged release.
- [ ] When a new binary is needed, publish the current API as a new pinned release rather than replacing `v0.1.0`. The curl installer selects tagged releases, not main; retain checksum and source-revision provenance. No rebuild/publication is needed for the current migration: the VPS already has source revision `129255618c45682d64398df87e905693232d43ad` (its existing binary still reports `dks 0.1.0`). CLI source is now `0.2.0-dev`; the independently versioned Nickel library is `0.3.0`.
