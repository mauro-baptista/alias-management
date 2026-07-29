# am — Alias Manager

`am` is a small Rust CLI that manages personal bash aliases in a single
dedicated file, `~/.alias-management`. It never touches aliases defined
anywhere else — the only other file it writes is `~/.bash_profile`, once,
during initial setup.

```
$ am
┌─────────┬───────┬────────────────────────┐
│ type    ┆ alias ┆ action                 │
╞═════════╪═══════╪════════════════════════╡
│ command ┆ gp    ┆ git pull               │
│ folder  ┆ p     ┆ cd ~/folder/personal   │
└─────────┴───────┴────────────────────────┘
```

## Install

```bash
cargo install --path .
```

Then run `am` once. The first run performs setup:

1. Creates `~/.alias-management` if it does not exist.
2. Ensures the shell-integration block (below) is present in
   `~/.bash_profile`, creating the file if needed. The block is
   marker-delimited and appended exactly once — re-running `am` never
   duplicates it.

Finally, restart your shell or run `source ~/.bash_profile`.

## Shell integration

A Rust binary runs in a child process, so it cannot `source` a file into
your running shell. Instead, setup installs this block into
`~/.bash_profile`:

```bash
# >>> alias-management (am) >>>
# Added by `am` (alias-management). Do not edit this block by hand.
# Loads managed aliases and re-sources them after every `am` run so
# changes take effect in the current shell immediately.
[ -f "$HOME/.alias-management" ] && source "$HOME/.alias-management"
am() {
    command am "$@"
    local am_status=$?
    if [ -f "$HOME/.alias-management" ]; then
        source "$HOME/.alias-management"
    fi
    return $am_status
}
# <<< alias-management (am) <<<
```

It does two things:

- sources `~/.alias-management` when the shell starts, and
- wraps `am` in a shell function: every `am` invocation runs the real
  binary (`command am`), then immediately re-sources
  `~/.alias-management` in the **active** shell — so an alias created
  with `am new` works right away, no new terminal needed.

If `am` has to create `~/.bash_profile` from scratch, it also adds a line
sourcing `~/.profile` first. bash reads only the first of
`~/.bash_profile` / `~/.bash_login` / `~/.profile` for login shells, so
without that line a brand-new `~/.bash_profile` would silently disable an
existing `~/.profile` (and any PATH setup in it).

## Storage format

Aliases are stored as standard bash, one per line, with a required
trailing comment marking the type:

```bash
alias gp="git pull" #command
alias p="cd ~/folder/personal" #folder
```

Anything else in the file — comments, blank lines, hand-written bash,
alias lines without a `#command`/`#folder` marker — is ignored and
preserved verbatim. Only marked lines are managed.

## Commands

| Command | What it does |
|---|---|
| `am` | Lists managed aliases in a table (`type \| alias \| action`), grouped by type, alphabetical within each group. |
| `am new` | Interactive creation. Asks command vs. folder, then name and action. Folder paths default to the current working directory. |
| `am delete [NAME]` | Deletes a managed alias. With `NAME` it is scriptable; without, it shows a picker. Only the matching line is removed. |
| `am --help` | Full help; `am new --help` and `am delete --help` for details. |

Before saving, `am new` verifies the name is free **system-wide** by
probing a login shell (`bash -lic 'command -v -- "$1"'`), so it refuses to
shadow existing binaries, builtins, functions, or aliases — including the
ones it manages and `am` itself.

## Behavior notes and limitations

- **bash only.** The integration targets `~/.bash_profile` by design.
  `~/.bash_profile` is read by *login* shells; if your terminal opens
  non-login shells, add `source ~/.bash_profile` to `~/.bashrc` yourself
  (am deliberately never edits `~/.bashrc`).
- **Deleting an alias cannot un-define it in shells where it is already
  loaded** — sourcing only adds or overwrites definitions. `am delete`
  prints a reminder; run `unalias NAME` in open shells or start a new one.
- Writes are atomic (temp file + rename), so a crash can never truncate
  `~/.alias-management` or `~/.bash_profile`.
- Actions containing **both** single and double quotes cannot be stored as
  a plain bash alias line and are rejected with an explanation. Folder
  paths with spaces are stored quoted (`alias p='cd "/a/My Dir"' #folder`);
  paths containing quotes, `$`, backticks, or backslashes are rejected.
