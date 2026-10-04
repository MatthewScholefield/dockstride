# Pinned Nickel evaluator

This build tree is `nickel-lang-core` **0.19.0**, from the crates.io package, upstream commit `76f2a1695c56713804026a06184d983f86f472fa` (`core/` in https://github.com/tweag/nickel). The upstream MIT license and original manifest are retained. All Rust sources, embedded standard-library files, build script, and optional-feature sources are included; upstream integration fixtures, benches, README, and their manifest targets/dev-dependencies are omitted because Dockstride consumes this as a dependency with default features disabled.

## Local repair

`src/eval/merge.rs`, `RevertClosurize for NickelValue`: the non-thunk branch uses `self.closurize(cache, Environment::new())` instead of returning `self`. Deserialized nested containers are closed values but not atomic constants; merging them into a recursive record requires wrapping them in thunks before computing the recursive environment. The existing thunk-reversion path and debug assertions remain unchanged, and constants still avoid allocation.

The consumer regression is `tests/nickel_boundary.rs::deserialized_nested_containers_preserve_merge_invariants`; the full boundary suite also exercises nested YAML candidates and ordinary Nickel export. The defect reproduces in an unmodified, single-source upstream evaluator with `(std.deserialize 'Yaml "a:\n  b: 1\n") & {x = 2}`, independent of Dockstride's import adapter.

## Standalone export compatibility

The checked-in project/library uses standard Nickel syntax and preserves ordinary backend-aware `nickel export --format yaml compose.ncl` behavior. However, the **unpatched published 0.19.0 core has this nested imported-container merge defect**; a standalone Nickel CLI must incorporate this repair or an upstream version that fixes it to be compatible with these nested YAML fixtures. Dockstride embeds the repaired pinned evaluator, so no external Nickel executable is required.
