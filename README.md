<p align="center">
  <img src="crates/app/assets/logo.svg" alt="Usine logo" width="140">
</p>

# Usine

Usine is a desktop app that orchestrates LLM coding agents (the `claude` and
`codex` CLIs) on a kanban board. Each card is a unit of work that moves through a
pipeline — design a plan, get it approved, implement it on a git worktree/branch,
open a pull request, address review comments, and merge — with the agents doing
the heavy lifting and you stepping in to answer questions and approve at the
gates.

## Install

You'll need the Rust toolchain — if you don't have `cargo` yet, install it via
[rustup](https://rustup.rs/) first. Usine is distributed as a Cargo package, so
a single command builds it from source and drops a `usine` binary into
`~/.cargo/bin` (which rustup already puts on your `PATH`):

```sh
cargo install --git https://github.com/sigma-studios/usine usine-app --locked
```

That's all you need on **macOS** and **Windows**. On **Linux**, install a couple
of system libraries first (the app uses your system's webview, and the git
integration links libgit2/openssl):

```sh
# Debian / Ubuntu
sudo apt install libwebkit2gtk-4.1-dev libssl-dev pkg-config
```

To update later, run the same `cargo install …` command again.

## Run

```sh
usine
```

Running `usine` launches the app **detached**: the window opens, your terminal
prompt returns immediately, and the app keeps running even if you close the
terminal — on every platform. (Set `USINE_NO_DETACH=1` to keep it in the
foreground and watch its logs instead.)

### Just want to look around?

Demo mode runs the whole pipeline with simulated agents — no agents launched, no
tokens spent, no network, and a separate database so it never touches your real
board:

```sh
USINE_SIM=1 usine
```

### Requirements for real runs

By default Usine drives the real backends, so it expects these installed and
authenticated on your `PATH`:

- the [`claude`](https://docs.claude.com/en/docs/claude-code) CLI and/or the
  `codex` CLI, depending on the provider you pick per card,
- `git`, and `gh` (the GitHub CLI) for the pull-request integration.

If you only want to explore the UI, use `USINE_SIM=1` — none of the above are
needed.

### Where your data lives

Projects, cards, and history are stored in a local database under your
platform's data directory:

- macOS: `~/Library/Application Support/dev.usine.usine/usine.db`
- Linux: `~/.local/share/usine/usine.db`
- Windows: `%APPDATA%\usine\usine\data\usine.db`

Demo mode (`USINE_SIM=1`) uses a separate `usine-demo.db` in the same location.

---

## Development

Usine is a standard Cargo workspace (Rust, edition 2021) built with
[Dioxus](https://dioxuslabs.com/). Clone it and work from the repo root:

```sh
git clone https://github.com/sigma-studios/usine
cd usine
```

### Hot reloading

Install the Dioxus CLI once, then use `dx serve` for a live-reloading dev loop —
edits to the UI and styles reload without a full restart:

```sh
cargo install dioxus-cli
dx serve --package usine-app
```

Dev builds (`dx serve`, `cargo run`) intentionally stay in the **foreground** so
hot-reload and live logs work — only the installed release binary detaches
itself.

### Build, run, test

```sh
cargo build --workspace                # build everything
cargo test  --workspace                # test everything

cargo run -p usine-app                 # run the app (real backends)
USINE_SIM=1 cargo run -p usine-app     # run in demo mode (simulated backends)
```

The `usine-cli` crate is a headless harness that drives the same core through
the executor, used for smoke tests and live integration tests:

```sh
cargo run -p usine-cli                       # simulated end-to-end pipeline
cargo run -p usine-cli github                # live GitHub forge test (throwaway repo)
cargo run -p usine-cli real-e2e              # full real run through the executor
cargo run -p usine-cli real-plan <dir> <task...>   # one real `claude` plan over <dir>
```

### Isolating instances (`USINE_DATA_DIR`)

`USINE_DATA_DIR=<path>` relocates the whole data directory — database,
worktrees, and attachments — letting a second instance run fully isolated from
the main one. An absolute path is recommended; it composes with `USINE_SIM` (the
demo DB name applies under whichever directory is active).

### Worktree conventions

When a project's setup/teardown commands are left blank, Usine auto-detects
`setup-worktree.sh` / `teardown-worktree.sh` (also under `scripts/`) in the
worktree and runs them. Anything these scripts write inside the worktree must be
gitignored — agent runs commit with `git add -A`.

### Running Usine inside Usine

Usine can manage its own repository as a project. Add the repo as a project, set
a validate command (e.g. `cargo test --workspace`), and set the run command to:

```sh
USINE_DATA_DIR="$PWD/.preview-data" cargo run -p usine-app
```

Each preview then launches a fully isolated instance whose state lives inside the
card's worktree and is disposed of with it. Two things to know:

- `.preview-data/` must stay gitignored (it is, in this repo) — otherwise the
  preview instance's database would be committed by the agent's finalize step.
- If you launch a second instance *without* `USINE_DATA_DIR`, it finds the main
  instance's database locked and falls back to an in-memory store (an error toast
  tells you); nothing is corrupted, but nothing persists either.

Merged changes don't reach the running app — rebuild and relaunch to pick them
up.

### Architecture

The workspace is split into three crates:

- **`usine-core`** (lib `usine_core`) — UI-agnostic domain logic. It owns the
  domain model, the pure card
  [state machine](crates/core/src/state_machine.rs), typed persistence (via
  `native_db`), git and forge (GitHub via `gh`) integration, the provider
  abstraction, and the async **executor** that ties them together. It has **no
  UI dependencies**.
- **`usine-app`** (bin `usine`) — the Dioxus desktop UI. A thin, reactive view
  over `usine-core`: it sends `CardCommand`s to the executor and renders the
  `ExecutorEvent`s it drains back.
- **`usine-cli`** (bin `usine-cli`) — a headless harness that drives the same
  core through the executor.

**Provider factory (Phase A vs Phase B).** A `ProviderFactory` is injected into
the executor; the executor logic is identical regardless of which it gets. Phase
A injects simulators (`SimFactory` / `SimForge` / `SimGit`) so the whole pipeline
runs with no agents, tokens, or network. Phase B injects the real backends
(`RealFactory` / `GhForge` / `RealGit`), which shell out to the real
`claude`/`codex` CLIs, `git`, and `gh`.

**Threading.** The executor runs on its own background thread with a dedicated
multi-threaded Tokio runtime, communicating with the UI entirely over channels
(an unbounded `CardCommand` channel in, an unbounded `ExecutorEvent` channel
back). This keeps the executor's async work off the UI's single-threaded Dioxus
runtime, which simply drains events in one place.

## License

MIT — see [LICENSE](LICENSE).
