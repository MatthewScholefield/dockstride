# TODOs

- [ ] Add explicit Swarm volume import/adoption support, with node placement, attached-consumer checks, data preservation and operator confirmation. Existing `--adopt-existing-volumes` is Compose-only; Swarm setup does not import volume data.
- [ ] Make setup's next-command hint backend-aware: `dks up` for Compose, `dks deploy` for Swarm, while keeping JSON output stable.
- [ ] Include source revision in version/diagnostic output so unreleased binaries can be identified without confusing them with a tagged release.
- [ ] Nonblocking production nit: replace repetitive deploy convergence dumps (five-service JSON arrays every two seconds on the failed run) with concise human progress emitted when state changes; retain detailed JSON via an explicit detail/verbose mode and preserve the machine-readable interface.
- [ ] Nonblocking production nit: improve failed-deploy diagnostics with the healthcheck startup window and relevant release logs, distinguishing active from historical task failures where applicable. The first deploy run failed after Budget's long guarded readiness startup, but a later task retry became healthy; investigate and explain this outcome without assuming a bug.
- [x] Published stable CLI `v0.2.1` from source revision `ad8205c`, fixing Swarm deploy validation after Compose adaptation. The current patch is installed on the server; successful Swarm setup was observed. Prior stable `v0.2.0` publication is complete.
