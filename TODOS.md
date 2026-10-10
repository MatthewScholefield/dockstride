# TODOs

- [ ] Add explicit Swarm volume import/adoption support, with node placement, attached-consumer checks, data preservation and operator confirmation. Existing `--adopt-existing-volumes` is Compose-only; Swarm setup does not import volume data.
- [ ] Make setup's next-command hint backend-aware: `dks up` for Compose, `dks deploy` for Swarm, while keeping JSON output stable.
- [ ] Include source revision in version/diagnostic output so unreleased binaries can be identified without confusing them with a tagged release.
- [ ] Publish stable CLI `v0.2.0` with Nickel library API `0.3.0` for repeatable pinned installs, retaining archive checksums and source-revision provenance. The curl installer selects tagged releases, not main; do not replace `v0.1.0`. Publication does not upgrade the VPS binary; its currently working source revision `129255618c45682d64398df87e905693232d43ad` remains unchanged until an explicit upgrade.
