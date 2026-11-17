# Memory profile of one Nyl invocation

**Status:** note, November 2026. It records where one invocation's memory
goes, the fix made with it, and what is left open for long-running operation
(M7). It describes measurements, not a contract.

## Why

CI's "Native tools (M4)" job ran out of memory while the scenario harness ran
every scenario at once. `run_bounded` in `nyl/tests/scenario_harness_test.rs`
now caps the harness. This note checks whether Nyl itself uses more memory per
invocation than it should.

## Method

- The `nyl` binary was built in release mode with the `test-kinds` and
  `test-clock` features, without LTO, with line tables, and unstripped
  (285 MB on disk, 47 MB stripped). The fake image and infrastructure kinds
  stand in for Docker and OpenTofu.
- Commands ran in a fresh world like the harness creates: a bare
  `project.git`, a clone, and `--author`. The projects were the end-to-end
  example (`nyl/tests/scenarios/examples/end-to-end/project`) and the
  promotion reference project. For the promotion project, 120 dev states were
  recorded first, each reconciled and attested.
- Peak RSS: `/usr/bin/time` is not installed, so a wrapper polled the
  process's `VmHWM` and `smaps_rollup` every millisecond and read
  `ru_maxrss` from `wait4`. All three agreed within 1 MiB. Heap composition
  came from `valgrind --tool=massif`.
- Tier 1: the scenario harness test binary, built in the `test` profile,
  ran `test_reached_scenarios_pass_in_tier_one` with `NYL_SCENARIO_JOBS`
  set to 1 and to 2.

## Measurements

Peak RSS per command, before and after the fix:

| Command (end-to-end project)    | Before  | After  |
| ------------------------------- | ------- | ------ |
| `validate`                      | 32 MiB  | 32 MiB |
| `state init -e dev`             | 32 MiB  | 31 MiB |
| `plan -e dev`                   | 35 MiB  | 33 MiB |
| `reconcile -e dev` (first)      | 310 MiB | 37 MiB |
| `reconcile -e dev` (no change)  | 309 MiB | 37 MiB |
| `status -e dev`                 | 33 MiB  | 32 MiB |
| `publish-tree --target static`  | 299 MiB | 28 MiB |

| Command (promotion project, 120 dev states) | Before  | After  |
| ------------------------------------------- | ------- | ------ |
| `get promotion-candidates dev-to-prod`      | 48 MiB  | 47 MiB |
| `promote dev-to-prod --dry-run`             | 49 MiB  | 47 MiB |
| `plan -e prod`                              | 32 MiB  | 31 MiB |
| `reconcile -e dev` (no change)              | 310 MiB | 38 MiB |

`publish-tree` exits 1 in this setup after rendering because no Git author is
configured. Its peak covers the render.

| Tier-1 harness (in-process, debug test binary of 282 MB) | Before    | After  | Wall time    |
| -------------------------------------------------------- | --------- | ------ | ------------ |
| `NYL_SCENARIO_JOBS=1`                                    | 627 MiB   | 88 MiB | 202 s → 107 s |
| `NYL_SCENARIO_JOBS=2`                                    | 1171 MiB  | 96 MiB | 132 s → 68 s  |

After the fix, the reconcile heap peak under massif is 2.7 MB. About 1 MB of
that is OpenSSL's certificate store. The rest of the roughly 30 MiB baseline
is mapped code, shared libraries, and thread stacks.

## Top consumers

1. **The executable fingerprint (fixed).** Every render cache records the
   `nyl` executable's SHA-256 as the `tool:nyl` dependency. The fingerprint
   was computed from `fs::read(current_exe())`, which holds the whole binary in
   memory, again for each `RenderCache`. That was 285 MB for this build and
   about 45 MB for a release build. In the tier-1 harness it was the debug test
   binary, read once per in-process step. In tier 2 it was the debug `nyl`
   binary, read by every rendering step. This dominated the harness's memory
   and accounts for the CI out-of-memory failure.
2. **Recorded states for promotion.** `read_history`/`states_of` keep up to
   `HISTORY_STATES` (100) full `EnvironmentSnapshot`s and, with `--revision`,
   up to `HISTORY_REVISION_STATES` (1000). Their attestation snapshots are
   read in batches of 32 but kept in memory. Unit documents are kept as parsed
   `serde_json::Value` trees. With four units this is about 130 KB per state,
   so 16 MB of heap for 120 states. The cost grows with the number of units
   times the number of states.
3. **Whole-tree state reads.** The Git store's `snapshot_at` and
   `snapshots_at` call `tree::read`, which loads every blob of the commit,
   then filter it with `tree::beneath(location.path)`. This is cheap while
   each environment has refs of its own. A state ref shared by many
   environments would read all of them for each snapshot.
4. **Process-lifetime caches.** These are small in one invocation but never
   freed: the Helm `lookup` chart scans (`SCANS`, `CHECKED` in
   `helm/lookup/declaration.rs`), `MATCHED`/`REPORTED` in
   `helm/lookup/model.rs`, the probed capabilities, and the deprecation
   warning sets. A chart directory is scanned once per process and never
   rescanned.

Not significant here: the `#[tokio::main]` multi-thread runtime and the
current-thread runtime made for each target render, `Repository::open` for
each state-store call, and the `helm` and `kyverno` version probes. Each
probe is one short child process per render cache.

## Change

`renderer_tool_dependencies` now gets the executable digest from
`executable_digest()`. That function hashes the file through a 64 KiB buffer
(`file_sha256_hex`) and memoizes the result in a process-wide `OnceLock`. The
digest is byte-for-byte the same, so existing cache records stay valid. A unit
test checks that the chunked digest equals the whole-file digest, including
across chunk boundaries.

## Open recommendations

- **Promotion history (before M7 or a larger `--revision` window).** Keep
  only what candidate selection needs from each state, such as receipts,
  values, and attestations, and drop full unit documents. Alternatively,
  evaluate candidates newest first and stop at the first one that satisfies
  the path. Then `--revision` over 1000 states does not hold 1000 parsed
  snapshots.
- **Scoped state reads.** Read only the subtree at `location.path` in
  `snapshot_at`/`snapshots_at` and `beneath`-style callers, rather than the
  whole commit tree, before environments share a state ref.
- **Long-running operation (M7).** Make the global caches above per session,
  or bound or clear them between runs. Key the chart scan by content or
  modification time for directories too. Keep the executable digest
  process-wide, because the running code cannot change. Open the state
  repository once per command instead of once per call if profiles show
  pack-index reloads.
- **Harness.** Keep `run_bounded`. With the fix, one tier-1 scenario job costs
  well under 100 MiB, so the default of half the cores is safe on CI runners.
