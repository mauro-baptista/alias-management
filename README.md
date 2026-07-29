# am — Alias Manager

`am` is a small command-line tool (a single Rust binary) that manages your
personal bash aliases in one dedicated file, `~/.alias-management`. It works
on **Linux and macOS** with bash. It never touches aliases defined anywhere
else — the only other file it writes is `~/.bash_profile`, once, during
initial setup.

```
$ am
┌─────────┬───────┬──────────────────────┐
│ type    ┆ alias ┆ action               │
╞═════════╪═══════╪══════════════════════╡
│ command ┆ gp    ┆ git pull             │
│ folder  ┆ p     ┆ cd ~/folder/personal │
│ ssh     ┆ srv   ┆ ssh forge@127.0.0.1  │
└─────────┴───────┴──────────────────────┘
```

It manages three kinds of aliases:

| type | what it does | example |
|---|---|---|
| `command` | run any shell command | `gp` → `git pull` |
| `folder` | jump to a directory | `p` → `cd ~/folder/personal` |
| `ssh` | connect to a server | `srv` → `ssh forge@127.0.0.1` |

## Install

### Option 1 — just download the `am` file and put it in your bin folder

`am` is a single self-contained executable — no runtime, no libraries, no
config. To use it system-wide, all you need to do is download the `am`
binary built for your platform and put it in a folder on your `PATH`. The
easiest way is to let it install itself:

```bash
chmod +x am        # make the downloaded file executable
./am install       # copies it to /usr/local/bin (or ~/.local/bin without root)
```

`sudo ./am install` forces the system-wide folder, and `./am install ~/bin`
installs into a folder of your choice — am tells you if that folder is not
on your PATH and exactly which line to add. Doing it by hand works just as
well:

```bash
# system-wide (needs sudo)
sudo cp am /usr/local/bin/ && sudo chmod +x /usr/local/bin/am

# or user-only (make sure the folder is on your PATH)
mkdir -p ~/.local/bin && cp am ~/.local/bin/ && chmod +x ~/.local/bin/am
```

The binary must match your OS and CPU: use a Linux build on Linux and a
macOS build on a Mac (and mind x86_64 vs arm64). On macOS, if Gatekeeper
blocks a downloaded binary, clear the quarantine flag with
`xattr -d com.apple.quarantine /usr/local/bin/am`.

### Option 2 — build from source

With Rust 1.85+ installed:

```bash
cargo build --release                    # produces target/release/am
./target/release/am install              # copies it onto your PATH
# or, if ~/.cargo/bin is on your PATH:
cargo install --path .
```

### First run

`am install` only places the binary. Run `am` once afterwards — that first
run sets everything up:

1. Creates `~/.alias-management` if it does not exist.
2. Adds the shell-integration block (see below) to `~/.bash_profile`,
   creating the file if needed — exactly once, never duplicated.

Then restart your terminal or run `source ~/.bash_profile`.

## How to use

### See your aliases

```bash
am
```

Prints the table shown above: `type | alias | action`, grouped by type
(commands, then folders, then ssh), alphabetical within each group.

### Create an alias

```bash
am new
```

Everything is asked interactively:

1. **Type** — pick `command`, `folder` or `ssh`.
2. **Alias name** — refused if the name is already taken, either by am or
   by anything else on your system (binaries, builtins, functions, other
   aliases), so an existing command is never shadowed.
3. **The details**, depending on the type:
   - *command*: the command to run, e.g. `git pull`.
   - *folder*: the folder path — defaults to the directory you are
     standing in, so `cd` into the folder first and just press Enter.
   - *ssh*: the user and the host. User `forge` and host `127.0.0.1`
     become the command `ssh forge@127.0.0.1`.

The alias is appended to `~/.alias-management` and — thanks to the shell
integration — works in the current shell immediately.

### Delete an alias

```bash
am delete          # pick from a list
am delete gp       # delete by name
```

Either way, `am` asks first — **No is the default**, so a stray Enter never
deletes anything:

```
? Do you want to delete command 'gp'? (y/n) › no
```

For scripts and other non-interactive use, skip the prompt with
`am delete gp --yes` (or `-y`). Deleting removes only that alias's line
from `~/.alias-management`; everything else in the file stays untouched.

### Install or update the binary

```bash
am install         # copy this binary to /usr/local/bin (or ~/.local/bin)
sudo am install    # force the system-wide folder
am install ~/bin   # or pick any folder
```

Handy after building from source or downloading a newer version: the
running `am` copies itself into place atomically. `am install` never
touches your dotfiles (safe under sudo) — run any other am command once to
set up the shell integration.

### Help

```bash
am --help
am new --help
am delete --help
am install --help
```

## How the shell integration works

A child process cannot change the shell that launched it, so the `am`
binary alone could never make a new alias appear in your open terminal.
Setup therefore installs this block into `~/.bash_profile`:

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

It sources your aliases when the shell starts, and wraps `am` in a function
that re-sources them right after every `am` command — that is what makes
`am new` take effect instantly.

If `am` has to create `~/.bash_profile` from scratch, it also adds a line
sourcing `~/.profile` first: bash reads only the first of `~/.bash_profile`
/ `~/.bash_login` / `~/.profile` for login shells, and without that line a
brand-new `~/.bash_profile` would silently disable an existing `~/.profile`
(and any PATH setup living there).

## Storage format

Plain bash, one alias per line, with a required trailing comment marking
the type:

```bash
alias gp="git pull" #command
alias p="cd ~/folder/personal" #folder
alias srv="ssh forge@127.0.0.1" #ssh
```

Anything else in the file — comments, blank lines, hand-written bash,
alias lines without a type marker — is ignored and preserved verbatim.
Because it is just bash, you can edit the file by hand too; `am` picks the
changes up on the next run.

## Linux and macOS notes

- **macOS**: Terminal.app and iTerm2 start *login* shells, so
  `~/.bash_profile` (and with it your aliases) loads automatically. The
  default shell on modern macOS is zsh; `am` targets bash — switch with
  `chsh -s /bin/bash`, or start `bash` inside your session.
- **Linux**: many desktop terminals start *non-login* shells, which skip
  `~/.bash_profile`. If your aliases only show up after `bash -l`, add
  `source ~/.bash_profile` to your `~/.bashrc` (am deliberately never
  edits `~/.bashrc` itself).

## Behavior notes and limitations

- Deleting an alias cannot un-define it in shells where it is already
  loaded — sourcing only adds or overwrites definitions. `am delete`
  prints a reminder; run `unalias NAME` in open shells or start a new one.
- Writes are atomic (temp file + rename), so a crash can never truncate
  `~/.alias-management` or `~/.bash_profile`.
- Actions containing **both** single and double quotes cannot be stored as
  a plain bash alias line and are rejected with an explanation. Folder
  paths with spaces are stored quoted; paths containing quotes, `$`,
  backticks, or backslashes are rejected. SSH users and hosts accept the
  usual safe characters (letters, digits, `.`, `_`, `-`, plus `:` for
  IPv6 hosts).
