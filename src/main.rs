//! `am` — a manager for personal bash aliases.
//!
//! Aliases live in `~/.alias-management` (plain bash, one alias per line with
//! a trailing `#command` / `#folder` / `#ssh` marker). A marker-delimited
//! block in `~/.bash_profile` sources that file and wraps `am` in a shell
//! function that re-sources it after every run, so changes apply to the
//! active shell immediately.

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command as ShellCommand, Stdio};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{ContentArrangement, Table};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};
use regex::Regex;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ALIAS_FILE_NAME: &str = ".alias-management";
const PROFILE_FILE_NAME: &str = ".bash_profile";

const ALIAS_FILE_HEADER: &str = "\
# Managed by `am` (alias-management).
# Lines of the form
#   alias <name>=\"<action>\" #command
#   alias <name>=\"<action>\" #folder
#   alias <name>=\"<action>\" #ssh
# are owned by am. Anything else in this file is left untouched.
";

const PROFILE_MARKER_BEGIN: &str = "# >>> alias-management (am) >>>";

const PROFILE_BLOCK: &str = r#"# >>> alias-management (am) >>>
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
"#;

// bash reads only the first of ~/.bash_profile, ~/.bash_login and ~/.profile
// for login shells. When am has to create ~/.bash_profile from scratch it
// would otherwise shadow an existing ~/.profile and silently drop the user's
// login environment (PATH additions and the like).
const PROFILE_CREATE_PREAMBLE: &str = "\
# Created by `am` (alias-management).
# Keep sourcing ~/.profile: bash skips it for login shells once
# ~/.bash_profile exists.
[ -f \"$HOME/.profile\" ] && source \"$HOME/.profile\"
";

// ---------------------------------------------------------------------------
// Regexes
// ---------------------------------------------------------------------------

/// A managed alias line: `alias <name>="<action>" #<type>` (single quotes
/// around the action are accepted too). Anything that does not match is
/// treated as unmanaged content and preserved verbatim.
static ALIAS_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^\s*alias\s+([A-Za-z0-9_][A-Za-z0-9_.-]*)=(?:"([^"]*)"|'([^']*)')\s*#\s*(command|folder|ssh)\s*$"#,
    )
    .expect("alias line regex is valid")
});

/// Valid alias names. This is also the injection gate: a name is only ever
/// handed to a subprocess after it has passed this check.
static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9_.-]*$").expect("name regex is valid"));

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "am",
    bin_name = "am",
    version,
    about = "Manage personal bash aliases stored in ~/.alias-management",
    long_about = "am manages a single set of personal bash aliases stored in \
~/.alias-management. It never touches aliases defined anywhere else.\n\n\
On every run it makes sure ~/.alias-management exists and that a small, \
marker-delimited integration block is present in ~/.bash_profile. That block \
sources your managed aliases and wraps `am` in a shell function which \
re-sources the file after every `am` command, so aliases created with \
`am new` work in the current shell immediately.\n\n\
Aliases are stored as plain bash with a trailing type marker:\n    \
alias gp=\"git pull\" #command\n    \
alias p=\"cd ~/folder/personal\" #folder\n    \
alias srv=\"ssh forge@127.0.0.1\" #ssh\n\n\
Run `am` with no arguments to list everything it manages.",
    after_help = "Examples:\n  \
am                 List managed aliases in a table\n  \
am new             Interactively create a new alias\n  \
am delete gp       Delete 'gp' after a confirmation prompt\n  \
am delete gp -y    Delete 'gp' without asking\n  \
am delete          Pick a managed alias to delete from a list\n\n\
Files:\n  \
~/.alias-management   Managed alias definitions (sourced by bash)\n  \
~/.bash_profile       Receives the am shell-integration block on first run"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(
        about = "Create a new alias interactively",
        long_about = "Create a new alias interactively.\n\n\
Asks whether the alias is for a command, a folder or an ssh connection, then \
prompts for the alias name and the details: the command text, the folder \
path (defaulting to the current directory), or the ssh user and host (user \
'forge' and host '127.0.0.1' are stored as `ssh forge@127.0.0.1`). Before \
saving, the name is checked against aliases already managed here and \
against everything visible in a login shell (binaries, builtins, functions \
and existing aliases), so an existing name is never shadowed. The new alias \
is appended to ~/.alias-management."
    )]
    New,
    #[command(
        about = "Delete a managed alias",
        long_about = "Delete a managed alias.\n\n\
With NAME, deletes that alias from ~/.alias-management. Without NAME, shows \
a list of managed aliases to pick from. Either way you are asked to confirm \
first (No is the default); pass --yes to skip the prompt, e.g. in scripts. \
Only the matching managed line is removed; every other line in the file is \
preserved exactly.\n\n\
Note: an alias already loaded in an open shell stays active there until you \
run `unalias NAME` or start a new shell."
    )]
    Delete {
        #[arg(
            value_name = "NAME",
            help = "Name of the managed alias to delete (omit to pick interactively)"
        )]
        name: Option<String>,
        #[arg(short = 'y', long = "yes", help = "Skip the confirmation prompt")]
        yes: bool,
    },
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

// Declaration order gives Command < Folder < Ssh, which is also
// alphabetical: listings group commands first, then folders, then ssh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AliasKind {
    Command,
    Folder,
    Ssh,
}

impl AliasKind {
    fn as_str(self) -> &'static str {
        match self {
            AliasKind::Command => "command",
            AliasKind::Folder => "folder",
            AliasKind::Ssh => "ssh",
        }
    }

    fn from_marker(s: &str) -> Option<Self> {
        match s {
            "command" => Some(AliasKind::Command),
            "folder" => Some(AliasKind::Folder),
            "ssh" => Some(AliasKind::Ssh),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AliasEntry {
    name: String,
    action: String,
    kind: AliasKind,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = home_dir()?;
    ensure_setup(&home)?;
    let alias_path = alias_file_path(&home);
    match cli.command {
        None => cmd_list(&alias_path),
        Some(Cmd::New) => cmd_new(&home, &alias_path),
        Some(Cmd::Delete { name, yes }) => cmd_delete(&alias_path, name, yes),
    }
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set; am needs it to locate ~/.alias-management")
}

fn alias_file_path(home: &Path) -> PathBuf {
    home.join(ALIAS_FILE_NAME)
}

fn profile_path(home: &Path) -> PathBuf {
    home.join(PROFILE_FILE_NAME)
}

/// Idempotent first-run setup: create `~/.alias-management` if missing and
/// make sure the shell-integration block is present in `~/.bash_profile`.
/// Notices go to stderr so stdout stays clean for command output.
fn ensure_setup(home: &Path) -> Result<()> {
    let alias_path = alias_file_path(home);
    if !alias_path.exists() {
        write_atomic(&alias_path, ALIAS_FILE_HEADER)?;
        eprintln!("Created {}", alias_path.display());
    }

    let profile = profile_path(home);
    let current = match fs::read_to_string(&profile) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", profile.display()));
        }
    };
    let creating = current.is_none();
    let base = current.unwrap_or_else(|| PROFILE_CREATE_PREAMBLE.to_string());
    if let Some(updated) = with_profile_block(&base) {
        write_atomic(&profile, &updated)?;
        if creating {
            eprintln!(
                "Created {} with the am shell integration (it also sources ~/.profile so your login environment is preserved)",
                profile.display()
            );
        } else {
            eprintln!(
                "Installed the am shell integration in {}",
                profile.display()
            );
        }
        eprintln!("Restart your shell or run: source ~/{PROFILE_FILE_NAME}");
    }
    Ok(())
}

/// Returns the new profile content with the integration block appended, or
/// `None` when the block is already installed. Never rewrites an existing
/// block; the markers are the source of truth.
fn with_profile_block(content: &str) -> Option<String> {
    if content.contains(PROFILE_MARKER_BEGIN) {
        return None;
    }
    let mut out = String::with_capacity(content.len() + PROFILE_BLOCK.len() + 2);
    out.push_str(content);
    if !content.is_empty() {
        if !content.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(PROFILE_BLOCK);
    Some(out)
}

// ---------------------------------------------------------------------------
// Parsing and formatting
// ---------------------------------------------------------------------------

fn parse_alias_line(line: &str) -> Option<AliasEntry> {
    let caps = ALIAS_LINE_RE.captures(line)?;
    let name = caps.get(1)?.as_str().to_string();
    let action = caps
        .get(2)
        .or_else(|| caps.get(3))
        .map(|m| m.as_str().to_string())?;
    let kind = AliasKind::from_marker(caps.get(4)?.as_str())?;
    Some(AliasEntry { name, action, kind })
}

/// Quote an action for a bash `alias` line: double quotes by default (the
/// spec format), single quotes when the action itself contains double quotes.
fn quote_action(action: &str) -> Result<String> {
    if !action.contains('"') {
        Ok(format!("\"{action}\""))
    } else if !action.contains('\'') {
        Ok(format!("'{action}'"))
    } else {
        bail!(
            "the action contains both single and double quotes and cannot be stored safely as a bash alias"
        )
    }
}

fn format_alias_line(entry: &AliasEntry) -> Result<String> {
    Ok(format!(
        "alias {}={} #{}",
        entry.name,
        quote_action(&entry.action)?,
        entry.kind.as_str()
    ))
}

fn is_valid_alias_name(name: &str) -> bool {
    NAME_RE.is_match(name)
}

fn validate_action(action: &str) -> Result<(), String> {
    let trimmed = action.trim();
    if trimmed.is_empty() {
        return Err("the action cannot be empty".into());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("the action cannot contain newlines or control characters".into());
    }
    if trimmed.contains('"') && trimmed.contains('\'') {
        return Err(
            "the action contains both ' and \" and cannot be stored as a bash alias".into(),
        );
    }
    Ok(())
}

/// Remove every managed line whose alias name matches `name`. All other
/// lines — comments, blanks, hand-written bash, alias lines without a type
/// marker — are kept verbatim. (`lines()` normalizes CRLF to LF; the file is
/// bash-managed, so LF is expected anyway.)
fn remove_entry_lines(content: &str, name: &str) -> (String, usize) {
    let mut kept: Vec<&str> = Vec::new();
    let mut removed = 0;
    for line in content.lines() {
        match parse_alias_line(line) {
            Some(entry) if entry.name == name => removed += 1,
            _ => kept.push(line),
        }
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, removed)
}

/// Command group first, then folders, then ssh; case-insensitive
/// alphabetical by alias name within each group.
fn sort_for_display(entries: &mut [AliasEntry]) {
    entries.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn expand_input_path(input: &str, home: &Path, cwd: &Path) -> PathBuf {
    let trimmed = input.trim();
    let expanded = shellexpand::tilde_with_context(trimmed, || home.to_str());
    let path = PathBuf::from(expanded.as_ref());
    if path.is_relative() {
        cwd.join(path)
    } else {
        path
    }
}

fn contract_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Build the `cd ...` action for a folder alias. Plain paths are stored in
/// the spec shape (`cd ~/folder/personal`); paths with spaces or other
/// specials fall back to a double-quoted absolute path, which survives the
/// single-quoted alias line and bash's alias expansion. Characters that bash
/// would still interpret inside double quotes make the path unstorable.
fn folder_action(expanded: &Path, home: &Path) -> Result<String> {
    let display = contract_home(expanded, home);
    if display
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_./~+@%=:,-".contains(c))
    {
        return Ok(format!("cd {display}"));
    }
    let absolute = expanded.display().to_string();
    if absolute.contains(['"', '\'', '$', '`', '\\']) {
        bail!(
            "the folder path contains characters (quotes, $, ` or \\) that cannot be stored safely as a bash alias"
        );
    }
    Ok(format!("cd \"{absolute}\""))
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

/// Build the action for an ssh alias: `ssh <user>@<host>`.
fn ssh_action(user: &str, host: &str) -> String {
    format!("ssh {}@{}", user.trim(), host.trim())
}

/// SSH user names: start with a letter, digit or '_', then letters, digits,
/// '.', '_' and '-'. The leading-character rule also guarantees the stored
/// `ssh <user>@<host>` argument can never look like an ssh option.
fn validate_ssh_user(user: &str) -> Result<(), String> {
    let trimmed = user.trim();
    if trimmed.is_empty() {
        return Err("the ssh user cannot be empty".into());
    }
    let first_ok = trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let rest_ok = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !first_ok || !rest_ok {
        return Err(
            "the ssh user must start with a letter, digit or '_' and may only \
                    contain letters, digits, '.', '_' and '-'"
                .into(),
        );
    }
    Ok(())
}

/// Hostnames, IPv4 or IPv6 addresses: letters, digits, '.', ':', '_' and '-'.
fn validate_ssh_host(host: &str) -> Result<(), String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err("the host cannot be empty".into());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".:_-".contains(c))
    {
        return Err(
            "the host may only contain letters, digits, '.', ':', '_' and '-' \
                    (a hostname, IPv4 or IPv6 address)"
                .into(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// System-wide duplicate check
// ---------------------------------------------------------------------------

/// Check whether `name` already resolves to anything in a login shell:
/// binaries, builtins, functions and aliases (including the ones managed
/// here, since `-l` loads ~/.bash_profile). Returns what the name resolves
/// to, `None` when it is free, or an error if bash could not be spawned.
///
/// The name has already been validated against [`NAME_RE`] and is passed as
/// a positional parameter — it is never interpolated into the shell command.
fn alias_exists_in_system(name: &str) -> Result<Option<String>> {
    let output = ShellCommand::new("bash")
        .arg("-lic")
        .arg(r#"command -v -- "$1""#)
        .arg("am-syscheck") // $0
        .arg(name) // $1
        .stdin(Stdio::null())
        .output()
        .context("failed to spawn bash for the system-wide alias check")?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first = stdout.lines().next().unwrap_or("").trim().to_string();
        Ok(Some(first))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Write via a sibling temp file + rename so a crash mid-write can never
/// truncate the target.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    fs::write(&tmp_path, contents)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn load_entries(path: &Path) -> Result<Vec<AliasEntry>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(content.lines().filter_map(parse_alias_line).collect())
}

fn append_entry(path: &Path, entry: &AliasEntry) -> Result<()> {
    let mut content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format_alias_line(entry)?);
    content.push('\n');
    write_atomic(path, &content)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_list(alias_path: &Path) -> Result<()> {
    let mut entries = load_entries(alias_path)?;
    if entries.is_empty() {
        println!("No aliases managed yet. Run `am new` to create one.");
        return Ok(());
    }
    sort_for_display(&mut entries);
    println!("{}", build_table(&entries));
    Ok(())
}

fn build_table(entries: &[AliasEntry]) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["type", "alias", "action"]);
    for entry in entries {
        table.add_row(vec![
            entry.kind.as_str(),
            entry.name.as_str(),
            entry.action.as_str(),
        ]);
    }
    table
}

fn cmd_new(home: &Path, alias_path: &Path) -> Result<()> {
    ensure_interactive()?;
    let theme = ColorfulTheme::default();

    let Some(kind_idx) = as_cancel(
        Select::with_theme(&theme)
            .with_prompt("Create an alias for a command, a folder or an ssh connection?")
            .items(["command", "folder", "ssh"])
            .default(0)
            .interact_opt(),
    )?
    .flatten() else {
        println!("Cancelled.");
        return Ok(());
    };
    let kind = match kind_idx {
        0 => AliasKind::Command,
        1 => AliasKind::Folder,
        _ => AliasKind::Ssh,
    };

    let existing = load_entries(alias_path)?;
    let name = loop {
        let input = as_cancel(
            Input::<String>::with_theme(&theme)
                .with_prompt("Alias name")
                .validate_with(|value: &String| -> Result<(), String> {
                    let v = value.trim();
                    if v.is_empty() {
                        return Err("the alias name cannot be empty".into());
                    }
                    if !is_valid_alias_name(v) {
                        return Err("alias names must start with a letter, digit or '_' and \
                                    contain only letters, digits, '_', '.', '-'"
                            .into());
                    }
                    if existing.iter().any(|e| e.name == v) {
                        return Err(format!(
                            "'{v}' is already managed by am; delete it first with: am delete {v}"
                        ));
                    }
                    Ok(())
                })
                .interact_text(),
        )?;
        let Some(input) = input else {
            println!("Cancelled.");
            return Ok(());
        };
        let candidate = input.trim().to_string();
        match alias_exists_in_system(&candidate) {
            Ok(Some(found)) => {
                eprintln!(
                    "An alias or command named '{candidate}' already exists: {found}. Pick another name."
                );
            }
            Ok(None) => break candidate,
            Err(e) => {
                eprintln!("warning: could not run the system-wide alias check ({e:#}); continuing");
                break candidate;
            }
        }
    };

    let action = match kind {
        AliasKind::Command => {
            let Some(command) = as_cancel(
                Input::<String>::with_theme(&theme)
                    .with_prompt("Command to run")
                    .validate_with(|value: &String| validate_action(value))
                    .interact_text(),
            )?
            else {
                println!("Cancelled.");
                return Ok(());
            };
            command.trim().to_string()
        }
        AliasKind::Folder => {
            let cwd = env::current_dir().context("failed to determine the current directory")?;
            loop {
                let Some(input) = as_cancel(
                    Input::<String>::with_theme(&theme)
                        .with_prompt("Folder path")
                        .default(cwd.display().to_string())
                        .interact_text(),
                )?
                else {
                    println!("Cancelled.");
                    return Ok(());
                };
                let expanded = expand_input_path(&input, home, &cwd);
                if !expanded.is_dir() {
                    let Some(proceed) = as_cancel(
                        Confirm::with_theme(&theme)
                            .with_prompt(format!(
                                "'{}' does not exist (or is not a directory). Create the alias anyway?",
                                expanded.display()
                            ))
                            .default(false)
                            .interact_opt(),
                    )?
                    .flatten() else {
                        println!("Cancelled.");
                        return Ok(());
                    };
                    if !proceed {
                        continue;
                    }
                }
                break folder_action(&expanded, home)?;
            }
        }
        AliasKind::Ssh => {
            let Some(user) = as_cancel(
                Input::<String>::with_theme(&theme)
                    .with_prompt("SSH user")
                    .validate_with(|value: &String| validate_ssh_user(value))
                    .interact_text(),
            )?
            else {
                println!("Cancelled.");
                return Ok(());
            };
            let Some(host) = as_cancel(
                Input::<String>::with_theme(&theme)
                    .with_prompt("Host")
                    .validate_with(|value: &String| validate_ssh_host(value))
                    .interact_text(),
            )?
            else {
                println!("Cancelled.");
                return Ok(());
            };
            ssh_action(&user, &host)
        }
    };

    let entry = AliasEntry { name, action, kind };
    let line = format_alias_line(&entry)?;
    append_entry(alias_path, &entry)?;
    println!("Saved: {line}");
    println!(
        "It is live in this shell already if the am wrapper is installed; new shells always pick it up."
    );
    Ok(())
}

fn cmd_delete(alias_path: &Path, name: Option<String>, yes: bool) -> Result<()> {
    let mut entries = load_entries(alias_path)?;
    let target = match name {
        Some(name) => match entries.iter().find(|e| e.name == name) {
            Some(entry) => entry.clone(),
            None => bail!("'{name}' is not managed by am (run `am` to see managed aliases)"),
        },
        None => {
            ensure_interactive()?;
            if entries.is_empty() {
                println!("No managed aliases to delete. Run `am new` to create one.");
                return Ok(());
            }
            sort_for_display(&mut entries);
            let items: Vec<String> = entries
                .iter()
                .map(|e| format!("{}  ({})  {}", e.name, e.kind.as_str(), e.action))
                .collect();
            let Some(index) = as_cancel(
                Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Which alias do you want to delete?")
                    .items(&items)
                    .default(0)
                    .interact_opt(),
            )?
            .flatten() else {
                println!("Cancelled.");
                return Ok(());
            };
            entries[index].clone()
        }
    };

    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("deleting needs confirmation; re-run with --yes to skip the prompt in scripts");
        }
        let confirmed = as_cancel(
            Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "Do you want to delete {} '{}'?",
                    target.kind.as_str(),
                    target.name
                ))
                .default(false)
                .interact_opt(),
        )?
        .flatten()
        .unwrap_or(false);
        if !confirmed {
            println!("Not deleted.");
            return Ok(());
        }
    }

    let content = fs::read_to_string(alias_path)
        .with_context(|| format!("failed to read {}", alias_path.display()))?;
    let (updated, removed) = remove_entry_lines(&content, &target.name);
    if removed == 0 {
        bail!(
            "'{}' is not managed by am (run `am` to see managed aliases)",
            target.name
        );
    }
    write_atomic(alias_path, &updated)?;
    println!("Deleted alias '{}' from ~/{ALIAS_FILE_NAME}.", target.name);
    println!(
        "Note: if '{0}' is active in an open shell, run `unalias {0}` there or start a new shell.",
        target.name
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt helpers
// ---------------------------------------------------------------------------

fn ensure_interactive() -> Result<()> {
    if std::io::stdin().is_terminal() {
        Ok(())
    } else {
        bail!("this command needs an interactive terminal")
    }
}

/// Map Ctrl-C during a prompt (an interrupted-IO error from dialoguer) to a
/// clean cancellation (`Ok(None)`) instead of a hard error.
fn as_cancel<T>(result: Result<T, dialoguer::Error>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(dialoguer::Error::IO(e)) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("terminal prompt failed")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, action: &str, kind: AliasKind) -> AliasEntry {
        AliasEntry {
            name: name.into(),
            action: action.into(),
            kind,
        }
    }

    // -- parsing --

    #[test]
    fn parse_spec_examples() {
        assert_eq!(
            parse_alias_line(r#"alias gp="git pull" #command"#),
            Some(entry("gp", "git pull", AliasKind::Command))
        );
        assert_eq!(
            parse_alias_line(r#"alias p="cd ~/folder/personal" #folder"#),
            Some(entry("p", "cd ~/folder/personal", AliasKind::Folder))
        );
        assert_eq!(
            parse_alias_line(r#"alias srv="ssh forge@127.0.0.1" #ssh"#),
            Some(entry("srv", "ssh forge@127.0.0.1", AliasKind::Ssh))
        );
    }

    #[test]
    fn parse_single_quoted_action() {
        assert_eq!(
            parse_alias_line(r#"alias p='cd "/home/u/My Dir"' #folder"#),
            Some(entry("p", r#"cd "/home/u/My Dir""#, AliasKind::Folder))
        );
    }

    #[test]
    fn parse_tolerates_whitespace_variants() {
        assert_eq!(
            parse_alias_line(r#"  alias gp="git pull"   #  command  "#),
            Some(entry("gp", "git pull", AliasKind::Command))
        );
        assert_eq!(
            parse_alias_line(r#"alias gp="git pull" # folder"#),
            Some(entry("gp", "git pull", AliasKind::Folder))
        );
    }

    #[test]
    fn parse_rejects_missing_type_comment() {
        assert_eq!(parse_alias_line(r#"alias gp="git pull""#), None);
    }

    #[test]
    fn parse_rejects_unknown_type() {
        assert_eq!(parse_alias_line(r#"alias gp="git pull" #dir"#), None);
    }

    #[test]
    fn parse_rejects_bad_name_or_spacing() {
        assert_eq!(parse_alias_line(r#"alias -x="y" #command"#), None);
        assert_eq!(parse_alias_line(r#"alias a b="y" #command"#), None);
        assert_eq!(parse_alias_line(r#"alias gp ="y" #command"#), None);
    }

    #[test]
    fn parse_rejects_mismatched_quotes() {
        assert_eq!(parse_alias_line(r#"alias x="y' #command"#), None);
    }

    // -- name validation --

    #[test]
    fn name_validation_charset() {
        for good in ["gp", "p2", "my_alias", "a.b-c", "_x", "9lives"] {
            assert!(is_valid_alias_name(good), "{good} should be valid");
        }
        for bad in [
            "", "-x", ".x", "a b", "rm;id", "$(x)", "a'b", "a\"b", "a\\b", "a$b",
        ] {
            assert!(!is_valid_alias_name(bad), "{bad} should be invalid");
        }
    }

    // -- formatting --

    #[test]
    fn format_prefers_double_quotes() {
        assert_eq!(
            format_alias_line(&entry("gp", "git pull", AliasKind::Command)).unwrap(),
            r#"alias gp="git pull" #command"#
        );
    }

    #[test]
    fn format_falls_back_to_single_quotes() {
        assert_eq!(
            format_alias_line(&entry("gc", r#"git commit -m "wip""#, AliasKind::Command)).unwrap(),
            r#"alias gc='git commit -m "wip"' #command"#
        );
    }

    #[test]
    fn format_rejects_both_quote_types() {
        assert!(format_alias_line(&entry("x", r#"echo "a" 'b'"#, AliasKind::Command)).is_err());
    }

    #[test]
    fn roundtrip_format_then_parse() {
        let cases = vec![
            entry("gp", "git pull", AliasKind::Command),
            entry("gc", r#"git commit -m "wip""#, AliasKind::Command),
            entry("p", "cd ~/folder/personal", AliasKind::Folder),
            entry("s", r#"cd "/home/u/My Dir""#, AliasKind::Folder),
            entry("srv", "ssh forge@127.0.0.1", AliasKind::Ssh),
        ];
        for original in cases {
            let line = format_alias_line(&original).unwrap();
            assert_eq!(parse_alias_line(&line), Some(original));
        }
    }

    // -- removal --

    #[test]
    fn remove_only_target_lines() {
        let content = "# header comment\n\
                       alias a=\"echo a\" #command\n\
                       alias gp=\"git pull\" #command\n\
                       alias p=\"cd ~/x\" #folder\n\
                       \n\
                       export FOO=1\n";
        let (out, removed) = remove_entry_lines(content, "gp");
        assert_eq!(removed, 1);
        assert_eq!(
            out,
            "# header comment\n\
             alias a=\"echo a\" #command\n\
             alias p=\"cd ~/x\" #folder\n\
             \n\
             export FOO=1\n"
        );
    }

    #[test]
    fn remove_missing_name_is_noop() {
        let content = "# c\nalias gp=\"git pull\" #command\n";
        let (out, removed) = remove_entry_lines(content, "zz");
        assert_eq!(removed, 0);
        assert_eq!(out, content);
    }

    #[test]
    fn remove_ignores_untyped_alias_with_same_name() {
        let content = "alias gp=\"x\"\nalias gp=\"git pull\" #command\n";
        let (out, removed) = remove_entry_lines(content, "gp");
        assert_eq!(removed, 1);
        assert_eq!(out, "alias gp=\"x\"\n");
    }

    // -- profile block --

    #[test]
    fn profile_block_appended_once() {
        let out = with_profile_block("").unwrap();
        assert!(out.starts_with(PROFILE_MARKER_BEGIN));
        assert_eq!(out.matches(PROFILE_MARKER_BEGIN).count(), 1);

        let out = with_profile_block("export PATH=$PATH:/x").unwrap();
        assert!(out.starts_with("export PATH=$PATH:/x\n\n# >>>"));
        assert_eq!(out.matches(PROFILE_MARKER_BEGIN).count(), 1);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn profile_block_idempotent() {
        let installed = with_profile_block("").unwrap();
        assert!(with_profile_block(&installed).is_none());
    }

    #[test]
    fn profile_block_shape() {
        assert!(PROFILE_BLOCK.starts_with(PROFILE_MARKER_BEGIN));
        assert!(
            PROFILE_BLOCK
                .trim_end()
                .ends_with("# <<< alias-management (am) <<<")
        );
        assert!(PROFILE_BLOCK.ends_with('\n'));
    }

    #[test]
    fn profile_created_fresh_sources_dot_profile() {
        let out = with_profile_block(PROFILE_CREATE_PREAMBLE).unwrap();
        assert!(out.contains(r#"[ -f "$HOME/.profile" ] && source "$HOME/.profile""#));
        assert!(out.contains(PROFILE_MARKER_BEGIN));
    }

    // -- path helpers --

    #[test]
    fn contract_home_variants() {
        let home = Path::new("/root");
        assert_eq!(contract_home(Path::new("/root/x"), home), "~/x");
        assert_eq!(contract_home(Path::new("/root"), home), "~");
        assert_eq!(contract_home(Path::new("/rootx"), home), "/rootx");
        assert_eq!(contract_home(Path::new("/other/y"), home), "/other/y");
    }

    #[test]
    fn expand_input_path_tilde_and_relative() {
        let home = Path::new("/h");
        let cwd = Path::new("/c");
        assert_eq!(expand_input_path("~/x", home, cwd), PathBuf::from("/h/x"));
        assert_eq!(expand_input_path("sub", home, cwd), PathBuf::from("/c/sub"));
        assert_eq!(expand_input_path("/abs", home, cwd), PathBuf::from("/abs"));
        assert_eq!(
            expand_input_path("  ~/x  ", home, cwd),
            PathBuf::from("/h/x")
        );
    }

    #[test]
    fn folder_action_plain_and_spaced() {
        let home = Path::new("/root");
        assert_eq!(
            folder_action(Path::new("/root/proj/p"), home).unwrap(),
            "cd ~/proj/p"
        );
        assert_eq!(
            folder_action(Path::new("/home/u/My Dir"), home).unwrap(),
            r#"cd "/home/u/My Dir""#
        );
        let spaced = entry(
            "p",
            &folder_action(Path::new("/home/u/My Dir"), home).unwrap(),
            AliasKind::Folder,
        );
        assert_eq!(
            format_alias_line(&spaced).unwrap(),
            r#"alias p='cd "/home/u/My Dir"' #folder"#
        );
        assert!(folder_action(Path::new("/home/u/it's"), home).is_err());
        assert!(folder_action(Path::new("/home/u/a$b c"), home).is_err());
    }

    // -- sorting and table --

    #[test]
    fn sort_groups_commands_then_folders_then_ssh() {
        let mut entries = vec![
            entry("s2", "ssh b@h", AliasKind::Ssh),
            entry("z", "cd ~/z", AliasKind::Folder),
            entry("B", "echo b", AliasKind::Command),
            entry("s1", "ssh a@h", AliasKind::Ssh),
            entry("a", "cd ~/a", AliasKind::Folder),
            entry("c", "echo c", AliasKind::Command),
        ];
        sort_for_display(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["B", "c", "a", "z", "s1", "s2"]);
    }

    #[test]
    fn table_contains_rows_in_order() {
        let entries = vec![
            entry("gp", "git pull", AliasKind::Command),
            entry("p", "cd ~/x", AliasKind::Folder),
            entry("srv", "ssh forge@127.0.0.1", AliasKind::Ssh),
        ];
        let rendered = build_table(&entries).to_string();
        for cell in ["type", "alias", "action", "command", "folder", "ssh"] {
            assert!(rendered.contains(cell), "table should contain '{cell}'");
        }
        let gp_pos = rendered.find("git pull").unwrap();
        let p_pos = rendered.find("cd ~/x").unwrap();
        let srv_pos = rendered.find("ssh forge@127.0.0.1").unwrap();
        assert!(
            gp_pos < p_pos && p_pos < srv_pos,
            "rows should be ordered command, folder, ssh"
        );
    }

    // -- action validation --

    #[test]
    fn action_validation() {
        assert!(validate_action("git pull").is_ok());
        assert!(validate_action(r#"git commit -m "wip""#).is_ok());
        assert!(validate_action("").is_err());
        assert!(validate_action("   ").is_err());
        assert!(validate_action("a\nb").is_err());
        assert!(validate_action(r#"echo "a" 'b'"#).is_err());
    }

    // -- ssh helpers --

    #[test]
    fn ssh_action_builds_user_at_host() {
        assert_eq!(ssh_action("forge", "127.0.0.1"), "ssh forge@127.0.0.1");
        assert_eq!(
            ssh_action("  forge  ", "  example.com  "),
            "ssh forge@example.com"
        );
        assert_eq!(
            format_alias_line(&entry(
                "srv",
                &ssh_action("forge", "127.0.0.1"),
                AliasKind::Ssh
            ))
            .unwrap(),
            r#"alias srv="ssh forge@127.0.0.1" #ssh"#
        );
    }

    #[test]
    fn ssh_user_validation() {
        for good in ["forge", "deploy_user", "user.name", "a-b", "_svc", "u2"] {
            assert!(validate_ssh_user(good).is_ok(), "{good} should be valid");
        }
        for bad in [
            "", "   ", "for ge", "forge@x", "-forge", "f$", "a'b", "a\"b",
        ] {
            assert!(validate_ssh_user(bad).is_err(), "{bad} should be invalid");
        }
    }

    #[test]
    fn ssh_host_validation() {
        for good in [
            "127.0.0.1",
            "example.com",
            "my-host_1",
            "2001:db8::1",
            "host",
        ] {
            assert!(validate_ssh_host(good).is_ok(), "{good} should be valid");
        }
        for bad in ["", "   ", "host name", "host/path", "$(x)", "a@b", "h'st"] {
            assert!(validate_ssh_host(bad).is_err(), "{bad} should be invalid");
        }
    }
}
