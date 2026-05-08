<p align="center">
    <img src="./.github/bolt-load-transparent.svg" alt="logo" width="200" height="200">
</p>

<div align="center">

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/greenhat616/bolt-load)

</div>

### Runner lifecycle API migration

The runner lifecycle refactor splits runner output into separate lifecycle and
data receivers. New callers should use `TaskRunner::builder().build()` and
consume the returned `(TaskRunner, LifecycleReceiver, DataFrameReceiver)`.
Legacy `TaskRunner::new`, `RunnerConnector`, `RunnerMessage`, and
`RunnerMessageConsumer` adapters are still available temporarily while task
implementations migrate to the new channels.

### Thread control algroithm

Split the process into 2 parts, slow start & max thread control

#### Slow start

Start with minimum thread 1, then for each time period, split each thread into
half. Stop the process when more thread can't get us significant boost. The
$THREASHOLD_1$ should be large, since this process is just rough estimate.

```math
\begin{align}
if \quad &|total\_speed_t - 2\times total\_speed_{t-1}| < THREASHOLD_1\\
then \quad &thread_t = 2\times thread_{t-1} \\
else \quad &break
\end{align}
```

#### Max thread control

When we reached $THREASHOLD_1$, instead of spliting every thread, choose one
task that has the longest unloaded range, then split the remaining chunk into to
tasks.

$THREASHOLD_2$ represents the minimum allowed speed of a connection, if adding a
thread can't gain enough speed, we'll stop adding new ones.

```math
\begin{align}
if \quad &|total\_speed_t - (total\_speed_{t-1} + thread\_average\_speed)| > THREASHOLD_2 \quad \&\& \quad thread_t < MAX\_THREADS \\
then \quad &thread_t = thread_{t-1} + 1 \\
else \quad & thread_t = thread_{t-1}
\end{align}
```

## Development

Install [Lefthook](https://lefthook.dev/) and [Deno](https://deno.com/) before
working on this repository.

```bash
lefthook install
```

The configured Git hooks run the following checks:

- `cargo fmt --all -- --check` for Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` for Rust linting.
- `deno fmt --check` for TypeScript and JavaScript formatting.
- `deno lint` for TypeScript and JavaScript linting.
- `deno run -A npm:@commitlint/cli --config commitlint.config.ts --edit {1}`
  for commit message validation.

Commit messages must follow the Conventional Commits style, for example:

```text
feat: add resumable download support
```
