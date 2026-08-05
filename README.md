# Shell . rs

A from-scratch, POSIX-inspired shell written in Rust. It provides an interactive REPL with built-in commands, external command execution via `$PATH`, pipes, redirection, background jobs, command chaining, tab completion, history with `HISTFILE` persistence, and shell variables with parameter expansion.

Everything — parsing a raw input line, dispatching it, and executing it — is built around a single typed `Command` enum.

---

## Features

- **REPL loop** — reads input, executes, repeats. Interactive raw-mode editing via `termion`.
- **Built-in commands**: `echo`, `type`, `exit`, `pwd`, `cd`, `jobs`, `history`, `declare`, and the `complete` compspec manager.
- **External command execution** — resolves executables through `$PATH` (with executable-bit checks) and runs `echo`-able arguments.
- **Quoting & tokenization** — a character-by-character state machine handling single quotes, double quotes, and backslash escaping.
- **I/O redirection** — `>`, `1>`, `2>`, `>>`, `2>>` (overwrite and append, stdout and stderr).
- **Tab completion**:
  - first word → command completion (built-ins + `$PATH` executables),
  - after the first word → file/directory completion,
  - custom `.autocomplete`-style scripts via `complete -C <script> <cmd>`.
- **Background jobs** — trailing `&`, a `jobs` builtin, and completion notifications.
- **Command chaining** — `cmd1 && cmd2` and `cmd1 || cmd2` with short-circuit semantics.
- **Pipes** — `cmd1 | cmd2 | ...` for built-ins and external commands alike.
- **History** — in-memory `history`, up/down arrow recall, and persistence via `HISTFILE` (`-r`, `-w`, `-a`).
- **Shell variables** — `declare Name=value` and `$Name` / `${Name}` parameter expansion.

---

## Built-ins

| Builtin    | Purpose |
|------------|---------|
| `echo`     | Print its arguments |
| `type`     | Report whether a name is a builtin, executable, or not found |
| `exit`     | Leave the shell |
| `pwd`      | Print the current working directory |
| `cd`       | Change directory (`~` for home, relative or absolute paths) |
| `jobs`     | List running and recently-finished background jobs |
| `complete` | Register (`-C`), print (`-p`), remove (`-r`) completion scripts |
| `history`  | Show command history, or read/write/append to a file (`-r`/`-w`/`-a`) |
| `declare`  | Set or print shell variables (`-p` prints, `Name=value` sets) |

---

## File structure

```
shell_rs/
├── Cargo.toml        # package manifest; deps: anyhow, termion
├── src/
│   ├── main.rs       # the REPL loop, command dispatch, and execution logic
│   └── cmd.rs        # the Command model: parsing, tokens, completion, shared stores
└── .gitignore
```

- **`main.rs`** — the forever-loop and the `match` that decides what each parsed command should *do*. It also owns the heavier runtime pieces (spawning external processes, running pipes and chains, monitoring background jobs).
- **`cmd.rs`** — the "brain": how a raw input line becomes a typed `Command` (parsing), plus every helper built on top of it (tokenization, parameter expansion, tab completion, history, and the in-memory stores).

---

## How it works

The whole design revolves around **one type: the `Command` enum.** It is the single source of truth — every feature is built on top of it.

### 1. `Command` — the core enum

Each line you type becomes one variant, carrying exactly the data its execution will need:

```rust
pub enum Command {
    ExitCommand,
    PwdCommand       { stdout_file: Option<String> },
    CdCommand        { directory: exeCmd },
    EchoCommand      { display_string: exeCmd },
    TypeCommand      { command_name: exeCmd },
    ExecCommand      { exec_name: exeCmd },
    CompleteCommand  { subcommnad: CompleteAction, stdout_file, stderr_file },
    JobsCommand      { stdout_file: Option<String> },
    HistoryCommand   { stdout_file, count, read_file, write_file, append_file },
    DeclareCommand   { subcommand: DeclareAction, stdout_file },
    CommandChain     { segments: Vec<ChainSegment>, background: bool },
    PipeCommand      { commands: Vec<Command>, background: bool },
    Noop,                              // empty input
    CommandNotFound,                   // produces an error message
}
```

Note that complex features reuse the same small set of building blocks:

- `exeCmd` — a reusable struct for "a command plus how to run it" (name, resolved `path`, `args`, stdout/stderr redirection, append flags, and a `background` flag). Built-ins and external commands both use it, so redirect and background support come "for free" everywhere.
- `ChainSegment { tokens, operator }` where `operator` is `And | Or | End` — the unit of a `&&` / `||` chain.
- `PipeCommand` holds a `Vec<Command>` — because each pipe segment is itself a full command, pipes work uniformly across built-ins and external executables.

### 2. From line to `Command` (`from_input`)

Parsing is one pipeline in `Command::from_input(&line)`:

```
raw input
  │  tokenize()                 ← quote/escape state machine → Vec<String>
  ▼
strip redirection operators     — pull out  >, 1>, 2>, >>, 2>>  + filenames
  ▼
expand $Name / ${Name}          — substitute shell-variable values
  ▼
detect & (background)           — trailing &  → background = true
  ▼
detect |  → PipeCommand       (any segment invalid ⇒ CommandNotFound)
detect && / ||  → CommandChain
  ▼
dispatch wildcard on first word → builtin variant, ExecCommand, or CommandNotFound
```

### 3. Execution (`main.rs`)

`main.rs` `match`es the returned `Command` and does the real work:

- runs built-ins directly (their output is deterministic, so no child process is needed),
- spawns external executables with `std::process::Command`,
- wires `Stdio::piped()` / `Stdio::inherit()` for pipes and redirects,
- for background jobs spawns a monitor thread that `wait()`s and flips job status.

### 4. Interactive layer + shared state

- `read_input_with_completion()` puts the terminal into raw mode to read keys one at a time — this is what powers tab completion, backspace, and ↑/↓ history recall.
- Long-lived data lives in **`OnceLock<Mutex<…>>` globals** (a small, dependency-light shared-state pattern):
  - `completions` — compspec map (command → completion script)
  - `history` + `history_written` — the history and a cursor into what's already on disk
  - `jobs` — the background job table
  - `shell_vars` — declared shell variables

Here's the loop at a glance:

```
              ┌─────────────────────────────────────────────┐
              │  REPL: prompt → read line → record history   │
              └───────────────┬─────────────────────────────┘
                              │
                         from_input()
                              │
              returns a Command ==========================┐
                              │                           │
                       main.rs matches                     │ Chain stored state
                              │                           │
   ┌──────────┬────────┬──────┴──────┬───────────┐        │
   │ builtin  │ external│   pipe      │ chain/job  │       │
   │ execute  │ spawn   │ wire Stdio  │ monitor    │◄──────┘
   └──────────┴─────────┴────────────┴────────────┘
```

---

## Advantages & Limitations

### Advantages

- **Memory-safe by construction** — no dangling pointers, use-after-free, or buffer overruns in a systems-level tool. The borrow checker catches them at compile time.
- **No garbage collector, no VM.** No runtime to warm up and no periodic GC pauses in the hot dispatch loop; memory usage stays small and predictable.
- **C-class performance with high-level ergonomics.** The default binary type is static and native — fast for both interactive use and heavy pipe workloads.
- **Explicit thread-as-possible concurrency.** Background jobs are reaped from monitor threads; `OnceLock<Mutex<…>>` stores keep shared state simple and race-free.
- **Small, understandable codebase.** One core enum + one dispatch loop — a good model for learning how shells are built.

### Limitations

- **In-memory pipe buffering.** Pipes are buffered in a `Vec<u8>` rather than wired to real OS pipes, so very large streams are memory-bound (fine for typical interactive use).
- **No nested pipes** — `a | b | c` returns `not found`; multi-segment pipes must not mix other operators.
- **No signal-based job control** — no `fg` / `bg` / `SIGSTOP`; background execution is spawn-and-monitor with a single PID reported.
- **Expansion precedes operator detection.** A variable whose value contains `|`, `&&`, or `&` is re-interpreted as syntax rather than passed through literally.
- **Single-quoting is tokenized, not tracked.** `$VAR` is expanded even inside single quotes.
- **Limited built-in job control / shell feature set** relative to Bash/zsh — the goal is a lean, correct shell, not a full replacement.

---

## Building & usage

Requires a Rust toolchain (`cargo`). There are only two dependencies: `anyhow` and `termion`.

### Build

```sh
# run an optimized release build
cargo build --release

# or just launch it directly (dev profile)
cargo run
```

### Start it

```sh
./target/release/shell_rs        # binary name matches the Cargo package; or: cargo run
```

The prompt shows the current directory. Type a command and press Enter.

### Example session

```sh
$ echo "hello world"
hello world

$ pwd
/home/you

$ ls -la                     # external command via $PATH
...

$ sleep 3 &                  # background job
[1] 2345

$ jobs
[1]<+> Running          sleep 3 &

$ declare Item=widget
$ echo stock_${Item}_id
stock_widget_id

$ ls | wc -l                # pipe an external into a builtin
42

$ history
    1  echo "hello world"
    2  declare Item=widget
```

### Persisting history

Set the `HISTFILE` environment variable to a file path before starting:

```sh
export HISTFILE="$HOME/.shell_rs_history"
cargo run
```

On startup the history is loaded from `HISTFILE`; new commands are appended when you exit.

---

## References

- [Code Crafters Shell challenge](https://app.codecrafters.io/courses/shell/introduction?repo=60248561-e862-421a-b1cb-48526d4e0ad9)
- [Grymoire UNIX Shell](https://www.grymoire.com/Unix/Sh.html)
