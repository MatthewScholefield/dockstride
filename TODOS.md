# TODOs

- [ ] Add explicit Swarm volume import/adoption support, with node placement, attached-consumer checks, data preservation and operator confirmation. Existing `--adopt-existing-volumes` is Compose-only; Swarm setup does not import volume data.
- [ ] Make setup's next-command hint backend-aware: `dks up` for Compose, `dks deploy` for Swarm, while keeping JSON output stable.
- [ ] Include source revision in version/diagnostic output so unreleased binaries can be identified without confusing them with a tagged release.
- [x] Published stable CLI `v0.2.0` with Nickel library API `0.3.0` from source revision `116f1a6fda2c686c88a9e1c7d770fb71317eb128`. Tag-triggered GitHub Actions run `38092546903` passed locked tests, independent reproducibility, package checksums and installer smoke tests on Linux x86_64/ARM64, then published archives with checksums and source-revision provenance. Verified real pinned (`DOCKSTRIDE_VERSION=v0.2.0`) and latest installs report `dks 0.2.0`; `v0.1.0` remains unchanged. Publication did not upgrade the VPS binary; its currently working source revision `129255618c45682d64398df87e905693232d43ad` remains unchanged until an explicit upgrade.
