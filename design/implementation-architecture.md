# Implementation architecture

**Status:** agreed direction for implementing M2–M7. See [ROADMAP.md](../ROADMAP.md)
and the contracts for [Release inputs](release-inputs.md), the
[orchestration core](orchestration-core.md), and
[infrastructure units](infrastructure-units.md).

The contracts define many rules. This document defines how the code is shaped
so each rule has one home, one test, and no way to drift from the others.
[AGENTS.md](../AGENTS.md) carries the lasting rules for contributors.

## Crates

Nyl is currently one crate. Orchestration is split into crates whose dependency
direction the compiler enforces:

```text
nyl-cli ─┬─► nyl-orchestration ─► nyl-core (types + traits)
         ├─► nyl-state ─────────► nyl-core
         ├─► nyl-drivers ─┬──────► nyl-core
         │                └──────► nyl-render ─► nyl-core
         └─► nyl-render
```

| Crate | Owns | Must not depend on |
| --- | --- | --- |
| `nyl-core` | Resource and state-file types, typed identifiers, schemas, canonical JSON and digests, JSON Pointer and reference resolution, and the traits orchestration uses (`StateStore`, `Driver`, `SourceRenderer`, `ArgoObserver`, `Clock`, `IdGenerator`) | Git, processes, network, clock, environment |
| `nyl-render` | Today's Kubernetes rendering and rendered GitOps, unchanged in behavior | Orchestration crates |
| `nyl-state` | The Git implementation of `StateStore`: snapshots, layout, transition commits, leases, run checkpoints, compare-and-swap | Drivers, rendering, orchestration |
| `nyl-drivers` | Implementations of `Driver`: execution context, process runner with masking, built-in unit kinds; the Kubernetes adapter uses `nyl-render` | State layout, orchestration |
| `nyl-orchestration` | Resolution, selection, lifecycle, recovery, promotion, teardown readiness, expiry | Everything except `nyl-core` |
| `nyl-cli` | Argument parsing, output formats, exit categories, and wiring the concrete implementations into orchestration | Business rules |

- Crate boundaries are the only boundaries the compiler enforces; module
  boundaries inside one crate erode.
- `nyl-render` compiles without any orchestration crate, which keeps
  orchestration optional in the code as well as in behavior.
- The `KubernetesPublication` driver is a thin adapter over a narrow rendering
  API, such as `render_target(target, input_snapshot) -> Tree`. Rendering never
  learns about environments, receipts, or state refs.
- `nyl-orchestration` depends only on `nyl-core`, so the compiler rejects any
  direct Git, process, or cluster call from orchestration; `nyl-cli` passes in
  the implementations from `nyl-state` and `nyl-drivers`.
- Plugin drivers, when added, implement the same driver trait through a
  process adapter in `nyl-drivers`.

## Pure decisions, thin shell

Most contract rules are decisions: whether a receipt is current, whether a unit
is ready, what a hold freezes, which publication a promotion takes, which
settings block a teardown, whether an instance has expired. They are pure
functions over an immutable snapshot:

```rust
fn plan(snapshot: &EnvironmentSnapshot, source: &RenderedSource, now: Timestamp) -> Decisions
```

- The shell in `nyl-orchestration` executes decisions and records results
  through the `StateStore` and `Driver` traits. It is the only code that
  performs effects.
- Time, identifiers such as UUIDs, and the process environment are inputs,
  never read globally. This matches the AGENTS.md rule that tests must not
  depend on global state.
- `nyl-cli` constructs the `Clock`. Built with the `test-clock` Cargo feature,
  it reads the current time from `NYL_TEST_NOW` and turns waits, such as a
  `delay` teardown wait, into clock advances instead of sleeps, so end-to-end
  tests of the real binary control time per child process. Release builds do
  not contain the feature and always use the system clock.
- Replay is the same function applied to history: the machine-audit
  requirement ("replaying operations reproduces every decision") becomes an
  executable check, and a later `nyl audit` can verify a state ref's history.

## Types that rule out illegal states

- Typed identifiers instead of strings: `EnvironmentName`, `UnitAddress`,
  `Uid`, `ExecutionKey`, `CommitId`, `RunId`, `JsonPointer`, `ChangeDigest`.
- Lifecycle and conditions are enums that carry their data, such as
  `Lifecycle::Held { hold, pending }`, `Lifecycle::Deleting { reason, intent }`,
  `Lifecycle::Retained { tombstone }`, and `Condition::Uncertain { run, since }`,
  so exhaustive matching forces every
  new state through every decision.
- Lifecycle transitions live in one table, `lifecycle::transition(current,
  event) -> Result<Next>`. Rules such as "a hold survives teardown" or "a kind
  change needs `--allow-teardown`" are one row and one test each.
- One implementation each for canonical JSON, digests, JSON Pointer resolution,
  reference resolution, and Git sources: one `GitSource` type
  (`repositoryRef`/`repository`, `revision`, `commit`, `path`) serves
  ApplicationGroup sources, `fromGit` bindings, unit sources and build
  contexts, and Environment sources, with per-use validation, and one module resolves and locks it. Bindings, PromotionPath selectors, and
  `nyl get output`/`get artifact` call the same code, so they cannot disagree.

## Rules have owners and tests

- Each rule in the contracts has one owning module, such as `rules::freshness`,
  `rules::execution_key`, `rules::approval`, `rules::lifecycle`,
  `rules::teardown_readiness`, `rules::teardown_wait`, or `rules::expiry`. Its doc comment links the
  contract section.
- Contract walkthroughs are executable scenarios against an in-memory
  `StateStore`, fake drivers, and a fake clock, for example
  `tests/scenarios/effects_without_receipt.yaml`. A contract change updates
  its scenario in the same change.
- The [reference scenarios](reference-scenarios.md) run whole workflows, from
  Git operations and clock changes to `nyl` invocations, against local bare
  repositories: in-process with fake tool drivers in every CI run, and through
  the real binary and tools where those are installed.
- Property tests (proptest) cover invariants: no unit executes with non-current
  provenance; a held unit's desired document never changes; replaying
  transition commits reproduces the state; a transition commit touches only its
  operation's paths.
- Crash injection at defined fault points in the shell (after an effect, before
  a checkpoint, before the final push) exercises recovery.
- Golden files cover every state-file kind and schema version, so format
  changes are deliberate.
- A driver conformance suite runs against every driver and, later, every plugin
  adapter: `Supported` handling, secret masking, deterministic execution-key
  inputs, and the declared recovery policy. Tests needing real Terraform,
  OpenTofu, or Docker are separate and gated on the tools being present.
- Git behavior is tested against local bare repositories in per-test temporary
  directories, parallel-safe as AGENTS.md requires.

## One result model for every output

- An operation produces one typed report: per unit its result, reason,
  approval, conditions, and warnings. Tables, `-o yaml`/`-o json`, the
  transition commit summary, `status`, and the exit category are all rendered
  from that report, so what people read, what CI parses, and what auditors
  replay agree by construction.
- Errors and warnings have stable codes with the fix attached, such as
  `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`, so the same problem reads the same in
  every place it appears.
- Each crate has its own error type; `nyl-cli` alone maps them to exit
  categories.

## Formats and traits

- State-file structs are versioned explicitly with migration functions; readers
  reject unknown versions. Schemas are generated from the Rust types through the
  existing schemars pipeline.
- Traits stay small: `StateStore`, `Driver`, `SourceRenderer`, `ArgoObserver`,
  `Clock`, `IdGenerator`. Each has an in-memory or fake implementation used by
  the scenario tests.
- Concurrency lives in one place: a wave executor with a bounded semaphore on
  the existing tokio runtime, and a single writer task for state commits.

## Getting there

- **M2** stays in today's rendering code and extracts `nyl-core` and
  `nyl-render` from the current crate with no behavior change, using the
  existing tests as the safety net.
- **M3** starts `nyl-state`, `nyl-orchestration`, and `nyl-drivers` as new
  crates, never inside `gitops/`, with the scenario harness and property tests
  from the first commit.
- **M4–M6** add drivers, the Kubernetes adapter, and promotion inside those
  boundaries; each contract walkthrough they introduce lands as a scenario.
