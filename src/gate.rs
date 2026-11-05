//! The push gate (DECISIONS.md D50, D46 phase 3).
//!
//! - **`git carve hooks install`** puts `git carve verify --pre-push` in git's own pre-push
//!   hook. That is the enforcement point: it covers every agent and every human.
//! - **`git carve hooks run <agent>`** is the courtesy layer, for agents' pre-tool hooks
//!   (Claude Code, Cursor, Codex). For a `git push`, it checks what the push would publish
//!   (fail-fast, D51). When a commit fails, it denies the push with the fix, and it always
//!   denies the bypasses (`--no-verify`, `-c core.hooksPath=…`). Anything else is allowed.
//!   Any carve error allows the push too (it fails open): the pre-push hook is what fails
//!   closed.
//!
//! carve never writes an agent's configuration: `hooks print` prints the snippets.

use std::fs;
use std::path::{Path, PathBuf};

use crate::branch::{self, VerifyOptions};
use crate::error::{Error, Result, io, precondition};
use crate::git::Git;
use crate::repo::Repo;

/// The pre-push hook's marker line: a hook without it is someone else's.
pub const MARKER: &str = "# git-carve pre-push gate";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Cursor,
    Codex,
}

impl Agent {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "cursor" => Some(Self::Cursor),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// A shell command an agent is about to run, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub command: String,
    pub cwd: Option<PathBuf>,
}

/// The command and working directory from an agent's hook input: `tool_input.command` for
/// Claude Code and Codex (`PreToolUse` on `Bash`), `command` for Cursor
/// (`beforeShellExecution`).
pub fn parse_event(agent: Agent, stdin: &str) -> Option<Event> {
    let doc: serde_json::Value = serde_json::from_str(stdin).ok()?;
    let command = match agent {
        Agent::Claude | Agent::Codex => doc.get("tool_input")?.get("command")?,
        Agent::Cursor => doc.get("command")?,
    }
    .as_str()?
    .to_owned();
    let cwd = doc.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
    Some(Event { command, cwd })
}

/// What a shell command does, as far as the gate cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Push {
    /// No `git push` in it.
    None,
    /// A `git push`: `-C` directories in order, and the revisions it would publish
    /// (`git log` arguments; empty means the branch's default range).
    Push {
        dirs: Vec<String>,
        revs: Vec<String>,
    },
    /// A push that would skip the pre-push hook, and how.
    Bypass(String),
}

/// Finds the first `git push` in a shell command line, best-effort: `&&`, `||`, `;`, `|`
/// and newlines separate commands; quotes are honored; leading `VAR=value`, `env`,
/// `command`, `exec` and `time` are skipped. An alias of push is not seen here; the
/// pre-push hook still catches it.
pub fn find_push(command: &str) -> Push {
    for words in simple_commands(command) {
        let mut rest = words.iter().map(String::as_str).peekable();
        while let Some(w) = rest.peek() {
            let assignment = w
                .split_once('=')
                .is_some_and(|(k, _)| !k.is_empty() && !k.starts_with('-'));
            if assignment || matches!(*w, "env" | "command" | "exec" | "time") {
                rest.next();
            } else {
                break;
            }
        }
        let Some(head) = rest.next() else { continue };
        if head != "git" && !head.ends_with("/git") {
            continue;
        }
        let mut dirs = Vec::new();
        let mut bypass = None;
        let mut sub = None;
        while let Some(w) = rest.next() {
            let config = match w {
                "-C" => {
                    dirs.extend(rest.next().map(str::to_owned));
                    None
                }
                "-c" | "--config-env" => rest.next(),
                "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix" => {
                    rest.next();
                    None
                }
                _ if w.starts_with("--config-env=") => w.strip_prefix("--config-env="),
                _ if w.starts_with('-') => None,
                _ => {
                    sub = Some(w);
                    break;
                }
            };
            if let Some(key) = config.map(|kv| kv.split('=').next().unwrap_or(kv)) {
                if key.eq_ignore_ascii_case("core.hooksPath") {
                    bypass = Some(format!("-c {key}=…"));
                }
            }
        }
        if sub != Some("push") {
            continue;
        }
        let args: Vec<&str> = rest.collect();
        if args.contains(&"--no-verify") {
            return Push::Bypass("--no-verify".to_owned());
        }
        if let Some(how) = bypass {
            return Push::Bypass(how);
        }
        return Push::Push {
            dirs,
            revs: push_revs(&args),
        };
    }
    Push::None
}

/// What a push's arguments would publish, as `git log` arguments; empty for the default.
fn push_revs(args: &[&str]) -> Vec<String> {
    if args
        .iter()
        .any(|a| matches!(*a, "--all" | "--mirror" | "--branches"))
    {
        return vec!["--branches".into(), "--not".into(), "--remotes".into()];
    }
    if args.iter().any(|a| matches!(*a, "-d" | "--delete")) {
        return vec!["--not".into(), "--all".into()]; // nothing new
    }
    let mut positional = Vec::new();
    let mut words = args.iter();
    while let Some(&w) = words.next() {
        match w {
            "--repo" | "-o" | "--push-option" | "--receive-pack" | "--exec" => {
                words.next();
            }
            _ if w.starts_with('-') => {}
            _ => positional.push(w),
        }
    }
    let srcs: Vec<String> = positional
        .iter()
        .skip(1) // the remote
        .filter_map(|spec| {
            let spec = spec.trim_start_matches('+');
            let src = spec.split(':').next().unwrap_or(spec);
            (!src.is_empty()).then(|| src.to_owned())
        })
        .collect();
    if srcs.is_empty() {
        return Vec::new();
    }
    let mut revs = srcs;
    revs.extend(["--not".to_owned(), "--remotes".to_owned()]);
    revs
}

/// After `>` or `<`: a second `>`/`<`, then either `&N`/`&-` (no file name) or a file name
/// word that the caller skips.
fn redirect(chars: &mut std::iter::Peekable<std::str::Chars>, skip_target: &mut bool) {
    while chars.peek().is_some_and(|c| matches!(c, '>' | '<')) {
        chars.next();
    }
    if chars.peek() == Some(&'&') {
        chars.next();
        while chars
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || *c == '-')
        {
            chars.next();
        }
    } else {
        *skip_target = true;
    }
}

/// Splits a command line into simple commands of words.
fn simple_commands(line: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    // The next word is a redirection's file name, not part of the command.
    let mut skip_target = false;
    let end_word =
        |words: &mut Vec<String>, word: &mut String, in_word: &mut bool, skip: &mut bool| {
            if *in_word {
                let w = std::mem::take(word);
                if !std::mem::take(skip) {
                    words.push(w);
                }
                *in_word = false;
            }
        };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for d in chars.by_ref() {
                    if d == '\'' {
                        break;
                    }
                    word.push(d);
                }
            }
            '"' => {
                in_word = true;
                while let Some(d) = chars.next() {
                    match d {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        _ => word.push(d),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            // Redirections (`2>&1`, `> out`, `&> log`, `<in`) are not words of the command.
            '>' | '<' => {
                if in_word && word.bytes().all(|b| b.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                end_word(&mut words, &mut word, &mut in_word, &mut skip_target);
                redirect(&mut chars, &mut skip_target);
            }
            '&' if chars.peek() == Some(&'>') => {
                end_word(&mut words, &mut word, &mut in_word, &mut skip_target);
                chars.next();
                redirect(&mut chars, &mut skip_target);
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut words, &mut word, &mut in_word, &mut skip_target);
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => {
                end_word(&mut words, &mut word, &mut in_word, &mut skip_target)
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut words, &mut word, &mut in_word, &mut skip_target);
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Allowed, with a one-line note on why the gate did not check (it fails open).
    AllowUnchecked(String),
    Deny(String),
}

/// The gate's decision for one agent event.
pub fn decide(event: &Event, env: &Git) -> Verdict {
    match find_push(&event.command) {
        Push::None => Verdict::Allow,
        Push::Bypass(how) => Verdict::Deny(format!(
            "git carve: `git push` with {how} skips the pre-push check that every pushed commit \
             passes. Push without it. If a commit fails the check, fix it, or ask the user to \
             re-split the unpushed commits with `git carve @{{u}}`."
        )),
        Push::Push { dirs, revs } => {
            let mut dir = event
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            for d in dirs {
                dir = dir.join(d);
            }
            match check_push(&dir, revs, env) {
                Ok(None) => Verdict::Allow,
                Ok(Some(reason)) => Verdict::Deny(reason),
                Err(e) => Verdict::AllowUnchecked(format!(
                    "git carve: push not checked ({})",
                    e.to_string().lines().next().unwrap_or_default()
                )),
            }
        }
    }
}

/// `verify --fail-fast` on what the push would publish: the reason to deny, if any.
fn check_push(dir: &Path, revs: Vec<String>, env: &Git) -> Result<Option<String>> {
    let repo = Repo::discover(env.with_dir(dir))?;
    let _lock = repo.lock()?;
    let ranges = if revs.is_empty() {
        branch::default_ranges(&repo)?
    } else {
        vec![revs]
    };
    let report = branch::verify_branch(
        &repo,
        &VerifyOptions {
            ranges,
            check: None,
            no_new_errors: branch::no_new_errors_config(&repo)?,
            fail_fast: true,
        },
    )?;
    let Some(bad) = report.commits.iter().find(|c| !c.passed) else {
        return Ok(None);
    };
    let upstream = repo
        .git
        .try_run_str(["rev-parse", "--verify", "-q", "@{upstream}"])?
        .is_some();
    let base = if upstream { "@{u}" } else { "<base branch>" };
    let mut reason = format!(
        "git carve: this push would publish a commit that does not pass the check (`{}`):\n  {} {}\n",
        report.check,
        &bad.oid[..bad.oid.len().min(9)],
        bad.subject
    );
    for e in &bad.errors {
        reason.push_str(&format!("    {e}\n"));
    }
    if let Some(e) = report.base_fails {
        reason.push_str(&format!(
            "The base already fails with {e} error(s). To check only for new ones in this repository: git config carve.noNewErrors true\n"
        ));
    }
    reason.push_str(&format!(
        "Every pushed commit must pass. Do not bypass this with --no-verify. Either:\n\
         - fix the code (amend the failing commit, or fix it in a new commit and have the user re-split), or\n\
         - have the user re-split the unpushed commits so each one passes: `git carve {base}`.\n\
         \x20 To help, run `git carve {base} --plan-only --json`, save its `buffer` to a file, put a suggested\n\
         \x20 message under each `commit` line as `# proposed: …` comments, and ask the user to run\n\
         \x20 `git carve {base} --plan-file <file> --edit`. The user writes the messages.\n"
    ));
    Ok(Some(reason))
}

/// The hook's response: (stdout, stderr, exit code). Claude Code and Codex deny with exit 2
/// and the reason on stderr; Cursor reads `{permission, user_message, agent_message}`.
pub fn respond(agent: Agent, verdict: &Verdict) -> (String, String, i32) {
    match (agent, verdict) {
        (Agent::Cursor, Verdict::Deny(reason)) => (
            serde_json::json!({
                "permission": "deny",
                "user_message": reason.lines().next().unwrap_or_default(),
                "agent_message": reason,
            })
            .to_string(),
            String::new(),
            2,
        ),
        (Agent::Cursor, Verdict::AllowUnchecked(note)) => (
            serde_json::json!({"permission": "allow"}).to_string(),
            format!("{note}\n"),
            0,
        ),
        (Agent::Cursor, Verdict::Allow) => (
            serde_json::json!({"permission": "allow"}).to_string(),
            String::new(),
            0,
        ),
        (_, Verdict::Deny(reason)) => (String::new(), reason.clone(), 2),
        (_, Verdict::AllowUnchecked(note)) => (String::new(), format!("{note}\n"), 0),
        (_, Verdict::Allow) => (String::new(), String::new(), 0),
    }
}

/// The pre-push hook `hooks install` writes. A missing git-carve skips the gate with a
/// warning rather than blocking every push.
pub fn pre_push_script() -> String {
    format!(
        "#!/bin/sh\n{MARKER}: every pushed commit must pass the check.\n\
         # Installed by `git carve hooks install`; remove with `git carve hooks uninstall`.\n\
         command -v git-carve >/dev/null 2>&1 || {{ echo \"git-carve not found; pre-push check skipped\" >&2; exit 0; }}\n\
         exec git-carve verify --pre-push\n"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    Written(PathBuf),
    Already(PathBuf),
}

/// Writes `pre_push_script` as the repository's pre-push hook. Refuses a foreign hook, and
/// a `core.hooksPath` (another tool manages hooks there; `hooks print git` shows the lines).
pub fn install(repo: &Repo) -> Result<Installed> {
    if let Some(path) = repo
        .git
        .try_run_str(["config", "core.hooksPath"])?
        .filter(|p| !p.is_empty())
    {
        return Err(precondition(
            format!(
                "core.hooksPath is set ({path}), so another tool manages this repository's hooks"
            ),
            Some("add the gate there yourself: `git carve hooks print git` shows the hook"),
        ));
    }
    let path = hook_path(repo)?;
    if let Ok(existing) = fs::read_to_string(&path) {
        if existing.contains(MARKER) {
            return Ok(Installed::Already(path));
        }
        return Err(precondition(
            format!("{} already exists and is not git carve's", path.display()),
            Some("add `git-carve verify --pre-push` to it yourself, or move it aside and re-run"),
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io(format!("create {}", dir.display())))?;
    }
    fs::write(&path, pre_push_script()).map_err(io(format!("write {}", path.display())))?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .map_err(io(format!("make {} executable", path.display())))?;
    Ok(Installed::Written(path))
}

/// Removes the pre-push hook if it is git carve's. `Ok(None)` when there was none to remove.
pub fn uninstall(repo: &Repo) -> Result<Option<PathBuf>> {
    let path = hook_path(repo)?;
    match fs::read_to_string(&path) {
        Ok(existing) if existing.contains(MARKER) => {
            fs::remove_file(&path).map_err(io(format!("remove {}", path.display())))?;
            Ok(Some(path))
        }
        Ok(_) => Err(Error::Precondition(format!(
            "{} is not git carve's; leaving it alone",
            path.display()
        ))),
        Err(_) => Ok(None),
    }
}

fn hook_path(repo: &Repo) -> Result<PathBuf> {
    let common = repo
        .git
        .run_str(["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(PathBuf::from(common.trim()).join("hooks").join("pre-push"))
}

/// The snippet `hooks print` shows for each target. carve never writes these files.
pub fn snippet(target: &str) -> Option<String> {
    let pre_tool = |agent: &str| {
        let mut hook = serde_json::json!({
            "type": "command",
            "command": format!("git-carve hooks run {agent}"),
            "timeout": 600
        });
        // Claude Code lets a timed-out hook through unless it says otherwise. A check that
        // outlasts the timeout must not let a push through (`hooks run` itself fails open on
        // its own errors, so this only bites on timeouts).
        if agent == "claude" {
            hook["onFailure"] = "block".into();
        }
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [hook]
                }]
            }
        }))
        .expect("static JSON")
    };
    Some(match target {
        "claude" => format!(
            "# Claude Code: merge into .claude/settings.json (this project) or ~/.claude/settings.json (you).\n# onFailure needs Claude Code 2.1.295 or later: a push whose check outlasts the timeout is blocked.\n{}\n",
            pre_tool("claude")
        ),
        "codex" => format!(
            "# Codex: merge into .codex/hooks.json (this project) or ~/.codex/hooks.json (you), then trust it with /hooks.\n{}\n",
            pre_tool("codex")
        ),
        "cursor" => format!(
            "# Cursor: merge into .cursor/hooks.json (this project) or ~/.cursor/hooks.json (you).\n{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": 1,
                "hooks": {"beforeShellExecution": [{"command": "git-carve hooks run cursor"}]}
            }))
            .expect("static JSON")
        ),
        "git" => format!(
            "# git: .git/hooks/pre-push (`git carve hooks install` writes it), or your hook manager's pre-push.\n{}",
            pre_push_script()
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(revs: &[&str]) -> Push {
        Push::Push {
            dirs: Vec::new(),
            revs: revs.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn finds_pushes_in_command_lines() {
        assert_eq!(find_push("git status && ls"), Push::None);
        assert_eq!(find_push("git push"), push(&[]));
        assert_eq!(
            find_push("npm test && git push -u origin HEAD"),
            push(&["HEAD", "--not", "--remotes"])
        );
        assert_eq!(
            find_push("GIT_TRACE=1 git -C sub push origin feat:main"),
            Push::Push {
                dirs: vec!["sub".into()],
                revs: vec!["feat".into(), "--not".into(), "--remotes".into()]
            }
        );
        assert_eq!(find_push("echo 'git push' | cat"), Push::None);
        assert_eq!(find_push("git commit -m \"push it\""), Push::None);
        assert_eq!(find_push("git push origin :old-branch"), push(&[]));
        assert_eq!(
            find_push("git push --all"),
            push(&["--branches", "--not", "--remotes"])
        );
        assert_eq!(
            find_push("git push --force-with-lease origin +feat"),
            push(&["feat", "--not", "--remotes"])
        );
        // D50 review: redirections are not revisions.
        let feat = push(&["feat", "--not", "--remotes"]);
        assert_eq!(find_push("git push origin feat 2>&1"), feat);
        assert_eq!(
            find_push("git push origin feat > out.log 2>&1 | tail"),
            feat
        );
        assert_eq!(find_push("git push origin feat &> log"), feat);
    }

    #[test]
    fn bypasses_are_named() {
        assert_eq!(
            find_push("git push --no-verify"),
            Push::Bypass("--no-verify".into())
        );
        assert_eq!(
            find_push("git -c core.hooksPath=/dev/null push"),
            Push::Bypass("-c core.hooksPath=…".into())
        );
        assert_eq!(
            find_push("git -c CORE.HOOKSPATH=x push origin main"),
            Push::Bypass("-c CORE.HOOKSPATH=…".into())
        );
        // Not a push: carve has no opinion.
        assert_eq!(
            find_push("git -c core.hooksPath=x commit --no-verify"),
            Push::None
        );
    }

    #[test]
    fn events_are_read_per_agent() {
        let claude = r#"{"tool_name":"Bash","tool_input":{"command":"git push"},"cwd":"/r"}"#;
        assert_eq!(
            parse_event(Agent::Claude, claude),
            Some(Event {
                command: "git push".into(),
                cwd: Some("/r".into())
            })
        );
        let cursor = r#"{"command":"git push","cwd":"/r"}"#;
        assert_eq!(
            parse_event(Agent::Cursor, cursor).unwrap().command,
            "git push"
        );
        assert_eq!(parse_event(Agent::Codex, "not json"), None);
    }

    #[test]
    fn responses_follow_each_agents_contract() {
        let deny = Verdict::Deny("no\nbecause".into());
        assert_eq!(
            respond(Agent::Claude, &deny),
            (String::new(), "no\nbecause".into(), 2)
        );
        let (out, _, code) = respond(Agent::Cursor, &deny);
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!((doc["permission"].as_str(), code), (Some("deny"), 2));
        assert_eq!(doc["user_message"], "no");
        assert_eq!(
            respond(Agent::Codex, &Verdict::Allow),
            (String::new(), String::new(), 0)
        );
    }
}
