use std::process::ExitCode;

use clap::Parser;
use git_carve::cli::{Arm, Cli, Command, EvalCommand, Split};
use git_carve::exit::Exit;
use git_carve::git::Git;
use git_carve::plan::{self, RenderOptions};
use git_carve::repo::Repo;
use git_carve::session::{self, Options, Summary};
use git_carve::{check, report, undo};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // --help and --version arrive here too; they are not errors.
            return if err.use_stderr() {
                Exit::Usage
            } else {
                Exit::Success
            }
            .into();
        }
    };
    match run(&cli) {
        Ok(exit) => exit.into(),
        Err(err) => {
            eprintln!("git carve: {err:#}");
            match err.downcast_ref::<git_carve::Error>() {
                Some(err) => err.exit().into(),
                None => Exit::Internal.into(),
            }
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<Exit> {
    if let Some(Command::Eval(cmd)) = &cli.command {
        return run_eval(cmd);
    }
    if let Some(Command::Hooks(cmd)) = &cli.command {
        return run_hooks(cmd);
    }
    let repo = Repo::discover(Git::new(std::env::current_dir()?))?;
    let _lock = repo.lock()?;
    if let Some(Command::Verify(args)) = &cli.command {
        return run_verify(&repo, args);
    }

    if cli.run.undo {
        let record = undo::undo(&repo)?;
        let mut pushed = Vec::new();
        for s in &record.stack {
            let name = s.ref_name.trim_start_matches("refs/heads/");
            if repo.git.test(["show-ref", "--verify", "-q", &s.ref_name])? {
                println!("Kept {name}: it moved since the carve.");
            }
            pushed.extend(git_carve::stack::on_remotes(&repo, name)?);
        }
        if !pushed.is_empty() {
            println!(
                "note: {} still exist on the remote, with any pull requests; carve never touches remotes.",
                pushed.join(", ")
            );
        }
        println!(
            "Undid {} commit(s): {} is back at {}. Your working tree was not touched.",
            record.commits.len(),
            record.ref_name.trim_start_matches("refs/heads/"),
            &record.old[..12]
        );
        return Ok(Exit::Success);
    }
    if cli.run.clean {
        let removed = check::clean(&repo)?;
        if removed.is_empty() {
            println!("Nothing to clean.");
        } else {
            println!("Removed: {}.", removed.join(", "));
        }
        return Ok(Exit::Success);
    }

    let base = match &cli.run.base {
        Some(expr) => {
            let base = git_carve::precondition::resolve_base(&repo, expr, cli.run.rewrite_pushed)?;
            if base.merge_based {
                eprintln!(
                    "note: {expr} is not an ancestor of HEAD; carving since {}, their merge-base",
                    &base.oid[..12]
                );
            }
            Some(base)
        }
        None => None,
    };
    let stack_base = cli.run.base.clone();
    // D48 --submit preflight: everything it needs, checked locally before any planning.
    let submit = if cli.run.submit {
        Some(submit_preflight(
            &repo,
            cli.run.base.as_deref().unwrap_or(""),
            base.as_ref(),
        )?)
    } else {
        None
    };
    let opts = Options {
        base,
        stack: cli.run.stack,
        check: cli.run.check.clone(),
        no_check: cli.run.no_check,
        plan_file: cli.run.plan_file.clone(),
        edit: cli.run.edit,
        judge: cli.run.judge.then(git_carve::judge::LearnedJudge::builtin),
        no_new_errors: cli.run.no_new_errors,
        jobs: None,
    };
    if let Some(judge) = opts.judge.as_ref().filter(|_| cli.run.verbose) {
        eprintln!("judge: {} (built in; {})", judge.name, judge.config());
    }
    if cli.run.plan_only {
        let prepared = session::prepare(&repo, &opts)?;
        if let Some(checker) = &prepared.checker {
            if cli.run.verbose {
                eprintln!("check: {}", checker.spec.command);
            }
        }
        let messages = vec![None; prepared.plan.groups.len()];
        let buffer = plan::render_range(
            &prepared.changes,
            &prepared.analysis,
            &prepared.plan,
            &RenderOptions {
                verification: prepared.verification.as_ref(),
                messages: &messages,
                notes: &[],
                diff_lines: if cli.run.verbose {
                    plan::DIFF_TOTAL_DEFAULT
                } else {
                    0
                },
                unsure: true,
            },
            opts.base
                .as_ref()
                .map(|base| plan::RangeView {
                    base,
                    sources: &prepared.sources,
                })
                .as_ref(),
        );
        if cli.run.json {
            let json = report::json_range(
                &prepared.changes,
                &prepared.analysis,
                &prepared.plan,
                prepared.verification.as_ref(),
                opts.base.as_ref(),
                &prepared.sources,
                Some(&buffer),
            );
            if cli.run.stack {
                print!(
                    "{}",
                    with_planned_stack(
                        &repo,
                        &json,
                        &stack_base,
                        opts.base.as_ref(),
                        prepared.plan.groups.len()
                    )?
                );
            } else {
                print!("{json}");
            }
        } else {
            print!("{buffer}");
            if let Some(v) = &prepared.verification {
                eprintln!(
                    "({} check runs, {} cached, {:.1}s)",
                    v.runs,
                    v.cached,
                    v.wall_ms as f64 / 1000.0
                );
            }
        }
        return Ok(Exit::Success);
    }

    let summary = session::run(&repo, &opts)?;
    if cli.run.json {
        return run_report(
            &repo,
            &summary,
            stack_base.as_deref().unwrap_or(""),
            submit.as_ref(),
        );
    }
    print_summary(&summary);
    if let Some(stack) = &summary.stack {
        print_stack(
            &repo,
            &summary,
            stack,
            stack_base.as_deref().unwrap_or(""),
            cli.run.verbose,
            submit.is_some(),
        )?;
        if let Some((trunk, _remote)) = &submit {
            if stack.adopted > 0 {
                eprintln!(
                    "git carve: --submit does not resubmit an adopted stack yet; your commits and layer branches are complete, run the commands above"
                );
                return Ok(Exit::SubmitFailed);
            }
            return Ok(submit_stack(stack, trunk, false).0);
        }
    }
    Ok(Exit::Success)
}

/// D48 `--submit` preflight, local only (no network): `gh` with the gh-stack extension, a
/// remote, and a known trunk. Returns (trunk, remote).
fn submit_preflight(
    repo: &Repo,
    base_expr: &str,
    base: Option<&git_carve::precondition::ResolvedBase>,
) -> anyhow::Result<(String, String)> {
    use git_carve::error::precondition;
    use git_carve::stack::{self, Tool};
    if stack::detect_tool() != Tool::GhStack {
        return Err(precondition(
            "--submit needs the GitHub CLI with the gh-stack extension",
            Some("`gh extension install github/gh-stack` (needs gh 2.90+), or drop --submit and run the printed commands"),
        )
        .into());
    }
    let remote = stack::remote_name(repo)?.ok_or_else(|| {
        precondition(
            "--submit needs a remote to push the stack to",
            Some("`git remote add origin <url>`"),
        )
    })?;
    let current = repo
        .git
        .try_run_str(["symbolic-ref", "-q", "--short", "HEAD"])?
        .unwrap_or_default();
    let oid = base.map_or("", |b| b.oid.as_str());
    let trunk = stack::trunk_name(repo, base_expr, oid, &current)?.ok_or_else(|| {
        precondition(
            format!("cannot tell which branch the stack goes on from `{base_expr}`"),
            Some("name the base branch, for example `git carve main --stack --submit`"),
        )
    })?;
    Ok((trunk, remote))
}

/// D48: runs `gh stack init` then `gh stack submit --auto`. carve passes no PR text: each
/// layer is one commit, so gh titles each PR with the human's subject. Exit 6 on failure.
/// `quiet` (`--json`) sends gh's output to stderr so stdout carries only the report.
fn submit_stack(
    stack: &git_carve::session::StackSummary,
    trunk: &str,
    quiet: bool,
) -> (Exit, Vec<String>) {
    let mut branches: Vec<String> = stack.layers.iter().map(|(b, _, _)| b.clone()).collect();
    branches.push(stack.branch.clone());
    let mut init: Vec<String> = vec![
        "stack".into(),
        "init".into(),
        "--base".into(),
        trunk.to_owned(),
    ];
    init.extend(branches.iter().cloned());
    let submit: Vec<String> = vec!["stack".into(), "submit".into(), "--auto".into()];
    let commands = vec![
        format!("gh {}", init.join(" ")),
        format!("gh {}", submit.join(" ")),
    ];
    for args in [&init, &submit] {
        let mut cmd = std::process::Command::new("gh");
        cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
        let ok = if quiet {
            match cmd.output() {
                Ok(out) => {
                    eprint!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    out.status.success()
                }
                Err(_) => false,
            }
        } else {
            println!("Running: gh {}", args.join(" "));
            cmd.status().is_ok_and(|s| s.success())
        };
        if !ok {
            eprintln!(
                "git carve: submitting the stack failed; your commits and layer branches are complete"
            );
            eprintln!(
                "retry (safe to repeat):\n  {}\n  {}",
                commands[0], commands[1]
            );
            return (Exit::SubmitFailed, commands);
        }
    }
    if !quiet {
        println!(
            "Submitted: {} draft PRs, one per layer (`gh stack submit --open` marks them ready).",
            branches.len()
        );
    }
    (Exit::Success, commands)
}

/// `--plan-only --json --stack` (D48): the planned layers. Their branch names take the
/// subjects typed in the editor, so only each layer's name prefix is known here.
fn with_planned_stack(
    repo: &Repo,
    json: &str,
    base_expr: &Option<String>,
    base: Option<&git_carve::precondition::ResolvedBase>,
    commits: usize,
) -> anyhow::Result<String> {
    use git_carve::stack;
    let mut doc: serde_json::Value = serde_json::from_str(json)?;
    let branch = repo
        .git
        .try_run_str(["symbolic-ref", "-q", "--short", "HEAD"])?;
    let planned = match &branch {
        None => serde_json::Value::Null,
        Some(branch) => {
            let oid = base.map_or("", |b| b.oid.as_str());
            let trunk = stack::trunk_name(repo, base_expr.as_deref().unwrap_or(""), oid, branch)?;
            let layers: Vec<serde_json::Value> = (1..commits)
                .map(|k| serde_json::json!({"k": k, "prefix": format!("{branch}-{k}")}))
                .chain(std::iter::once(
                    serde_json::json!({"k": commits, "branch": branch, "top": true}),
                ))
                .collect();
            serde_json::json!({
                "trunk": trunk,
                "remote": stack::remote_name(repo)?,
                "tool": stack::detect_tool(),
                "layers": layers,
            })
        }
    };
    doc["stack"] = planned;
    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

/// `--json` on a full run (D48): what was created, as a report agents can rely on.
fn run_report(
    repo: &Repo,
    s: &Summary,
    base_expr: &str,
    submit: Option<&(String, String)>,
) -> anyhow::Result<Exit> {
    use git_carve::stack;
    use serde_json::json;
    let commits: Vec<serde_json::Value> = s
        .record
        .commits
        .iter()
        .zip(&s.subjects)
        .map(|(oid, subject)| json!({"oid": oid, "subject": subject}))
        .collect();
    let mut exit = Exit::Success;
    let stack = match &s.stack {
        None => serde_json::Value::Null,
        Some(st) => {
            let base_oid = s.range.as_ref().map_or("", |r| r.oid.as_str());
            let trunk = stack::trunk_name(repo, base_expr, base_oid, &st.branch)?;
            let remote = stack::remote_name(repo)?;
            let tool = stack::detect_tool();
            let mut branches: Vec<String> = st.layers.iter().map(|(b, _, _)| b.clone()).collect();
            branches.push(st.branch.clone());
            let mut layers: Vec<serde_json::Value> = st
                .layers
                .iter()
                .enumerate()
                .map(|(k, (b, oid, subject))| {
                    json!({"k": k + 1, "branch": b, "oid": oid, "subject": subject, "adopted": k < st.adopted})
                })
                .collect();
            layers.push(json!({
                "k": branches.len(),
                "branch": st.branch,
                "oid": s.record.new,
                "subject": s.subjects.last(),
                "top": true,
            }));
            let submitted = match submit {
                Some(_) if st.adopted > 0 => {
                    exit = Exit::SubmitFailed;
                    json!({
                        "ran": false,
                        "ok": false,
                        "reason": "--submit does not resubmit an adopted stack yet",
                        "commands": adopted_commands(repo, st, &branches, trunk.as_deref(), remote.as_deref())?,
                    })
                }
                None if st.adopted > 0 => json!({
                    "ran": false,
                    "ok": null,
                    "commands": adopted_commands(repo, st, &branches, trunk.as_deref(), remote.as_deref())?,
                }),
                Some((trunk, _)) => {
                    let (code, commands) = submit_stack(st, trunk, true);
                    exit = code;
                    json!({"ran": true, "ok": code == Exit::Success, "commands": commands})
                }
                None => json!({
                    "ran": false,
                    "ok": null,
                    "commands": stack::commands(tool, trunk.as_deref(), remote.as_deref(), &branches),
                }),
            };
            json!({
                "trunk": trunk,
                "remote": remote,
                "tool": tool,
                "layers": layers,
                "stale": st.stale,
                "submit": submitted,
            })
        }
    };
    let report = json!({
        "version": 1,
        "branch": s.record.ref_name.trim_start_matches("refs/heads/"),
        "commits": commits,
        "left": s.record.left,
        "verified": s.verification.as_ref().map(|v| v.check.clone()),
        "range": s.range.as_ref().map(|r| json!({"base": r.oid, "commits": r.commits.len()})),
        "stack": stack,
        "undo": "git carve --undo",
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(exit)
}

/// D48: the stack's layers and how to submit them (nothing is pushed here).
fn print_stack(
    repo: &Repo,
    s: &Summary,
    stack: &git_carve::session::StackSummary,
    base_expr: &str,
    verbose: bool,
    submitting: bool,
) -> anyhow::Result<()> {
    use git_carve::stack::{self, Tool};
    let base_oid = s.range.as_ref().map_or("", |r| r.oid.as_str());
    let trunk = stack::trunk_name(repo, base_expr, base_oid, &stack.branch)?;
    let remote = stack::remote_name(repo)?;
    let mut branches: Vec<String> = stack.layers.iter().map(|(b, _, _)| b.clone()).collect();
    branches.push(stack.branch.clone());
    let checked = match &s.verification {
        Some(v) => format!("each passed `{}`", v.check),
        None => "NOT verified (--no-check)".to_owned(),
    };
    println!(
        "Stack on {} ({} layers, {checked}):",
        trunk.as_deref().unwrap_or("<trunk>"),
        branches.len()
    );
    let width = branches.iter().map(String::len).max().unwrap_or(0);
    let mut replaces = Vec::new();
    for (b, oid, subject) in &stack.layers {
        println!("  {b:width$}  {}  {subject}", &oid[..7]);
        replaces.extend(stack::on_remotes(repo, b)?);
    }
    let top = s.record.commits.last().map_or("", |c| &c[..7]);
    let top_subject = s.subjects.last().map_or("", String::as_str);
    println!("  {:width$}  {top}  {top_subject}   (top)", stack.branch);
    if let Some(upstream) =
        repo.git
            .try_run_str(["rev-parse", "-q", "--verify", "--abbrev-ref", "@{u}"])?
    {
        let up_oid = repo.git.run_str(["rev-parse", "@{u}"])?;
        if !repo
            .git
            .test(["merge-base", "--is-ancestor", &up_oid, &s.record.new])?
        {
            println!(
                "note: {} diverged from {upstream}; submitting force-pushes it (with --force-with-lease)",
                stack.branch
            );
        }
    }
    if !replaces.is_empty() && stack.adopted == 0 {
        println!(
            "note: {} already exist; pushing the new layers replaces them",
            replaces.join(", ")
        );
    }
    if !stack.stale.is_empty() {
        println!(
            "note: {} no longer in the stack; carve left them as they were (close their PRs)",
            stack.stale.join(", ")
        );
    }
    if stack.adopted > 0 {
        println!(
            "To update the stack's PRs ({} of {} layers kept their branch; nothing was pushed):",
            stack.adopted,
            branches.len()
        );
        for c in adopted_commands(repo, stack, &branches, trunk.as_deref(), remote.as_deref())? {
            println!("  {c}");
        }
        return Ok(());
    }
    if submitting {
        return Ok(());
    }
    let tool = stack::detect_tool();
    let tools: &[Tool] = if verbose {
        &[Tool::GhStack, Tool::Plain]
    } else {
        std::slice::from_ref(&tool)
    };
    for &t in tools {
        let label = match t {
            Tool::GhStack => "To submit with gh stack (nothing was pushed):",
            Tool::Plain => "To open the PRs without gh stack (nothing was pushed):",
        };
        println!("{label}");
        if trunk.is_none() {
            println!("  # replace <trunk> with the branch the stack goes on");
        }
        for c in stack::commands(t, trunk.as_deref(), remote.as_deref(), &branches) {
            println!("  {c}");
        }
    }
    Ok(())
}

/// D52: the commands that update an adopted stack's PRs. The adopted layers moved, so they
/// need a force-push; each lease pins the remote commit carve saw, so a push someone else made
/// in between is refused, not overwritten.
fn adopted_commands(
    repo: &Repo,
    stack: &git_carve::session::StackSummary,
    branches: &[String],
    trunk: Option<&str>,
    remote: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let remote = remote.unwrap_or("<remote>");
    let mut push = format!("git push -u {remote}");
    for b in branches {
        let tracked = format!("refs/remotes/{remote}/{b}");
        if let Some(oid) = repo
            .git
            .try_run_str(["rev-parse", "-q", "--verify", &tracked])?
        {
            push.push_str(&format!(" --force-with-lease={b}:{oid}"));
        }
    }
    push.push(' ');
    push.push_str(&branches.join(" "));
    let mut out = vec![push];
    let mut below = trunk.unwrap_or("<trunk>");
    for (k, b) in branches.iter().enumerate() {
        if k >= stack.adopted && k + 1 < branches.len() {
            out.push(format!("gh pr create --base {below} --head {b} --fill"));
        }
        below = b;
    }
    // The top branch's PR targeted the old top layer, which changed with the layer count.
    if stack.adopted != stack.layers.len() || !stack.stale.is_empty() {
        let under = match branches.len().checked_sub(2) {
            Some(i) => branches[i].as_str(),
            None => trunk.unwrap_or("<trunk>"),
        };
        out.push(format!("gh pr edit {} --base {under}", stack.branch));
    }
    Ok(out)
}

/// SPEC §10.4. Subjects are the user's own first lines.
fn print_summary(s: &Summary) {
    let n = s.record.commits.len();
    let place = if s.record.detached {
        "detached HEAD".to_owned()
    } else {
        s.record
            .ref_name
            .trim_start_matches("refs/heads/")
            .to_owned()
    };
    let plural = if n == 1 { "" } else { "s" };
    if let Some(range) = &s.range {
        let k = range.commits.len();
        println!(
            "Re-carved {k} commit{} since {} into {n}.",
            if k == 1 { "" } else { "s" },
            &range.oid[..7]
        );
    }
    match &s.verification {
        Some(v) => {
            let runs = match v.cached {
                0 => format!("{} check runs", v.runs),
                c => format!("{} check runs, {c} cached", v.runs),
            };
            let passed = match v.baseline_errors {
                Some(e) => format!(
                    "each adds no error beyond the {e} {} already had under `{}`",
                    if s.range.is_some() {
                        "the base"
                    } else {
                        "HEAD"
                    },
                    v.check
                ),
                None => format!("each passed `{}`", v.check),
            };
            println!(
                "Created {n} commit{plural} on {place}, {passed} ({runs}, {} merge{}, {:.1}s):",
                s.merges,
                if s.merges == 1 { "" } else { "s" },
                v.wall_ms as f64 / 1000.0
            )
        }
        None => {
            println!("Created {n} commit{plural} on {place}. They are NOT verified (--no-check):")
        }
    }
    for (oid, subject) in s.record.commits.iter().zip(&s.subjects) {
        println!("  {} {subject}", &oid[..7]);
    }
    if s.record.plan_source.as_deref() == Some("plan-file") {
        println!("Their messages came from the plan file, not the editor (carve.agentMessages).");
    }
    if !s.record.left.is_empty() {
        let hunks = s.record.left.len();
        println!(
            "Left in working tree: {hunks} hunk{} ({})",
            if hunks == 1 { "" } else { "s" },
            s.record.left.join(", ")
        );
    }
    for m in &s.missing {
        println!("warning: {m} was not in the plan, so it stays in your working tree");
    }
    if let Some(v) = &s.verification {
        if let Some(f) = &v.failing {
            println!(
                "note: your working tree does not pass `{}`, so the hunks it rejects stayed out: {}",
                v.check,
                f.errors.first().map_or("", String::as_str)
            );
        }
        for path in &v.unverified {
            println!("warning: the check does not read {path}; changes to it are unverified");
        }
    }
    println!("Undo: git carve --undo");
}

/// D50: `git carve hooks …`. `run` never needs a repository: it fails open.
fn run_hooks(cmd: &git_carve::cli::HooksCommand) -> anyhow::Result<Exit> {
    use git_carve::cli::HooksCommand;
    use git_carve::gate;
    use std::io::{Read, Write};
    match cmd {
        HooksCommand::Run { agent } => {
            let agent = gate::Agent::parse(agent).expect("clap checked the name");
            // The agent runs this before every shell command, and a snippet may set
            // `onFailure: block`, so an unread event or a crash must still answer: fail open
            // (D50). Timeouts are the agent's to handle.
            let mut stdin = String::new();
            let verdict = match std::io::stdin().read_to_string(&mut stdin) {
                Err(e) => gate::Verdict::AllowUnchecked(format!("could not read the event: {e}")),
                Ok(_) => std::panic::catch_unwind(|| match gate::parse_event(agent, &stdin) {
                    Some(event) => gate::decide(&event, &Git::new(".")),
                    None => gate::Verdict::Allow,
                })
                .unwrap_or_else(|_| gate::Verdict::AllowUnchecked("git carve crashed".into())),
            };
            let (out, err, code) = gate::respond(agent, &verdict);
            print!("{out}");
            eprint!("{err}");
            std::io::stdout().flush()?;
            std::process::exit(code);
        }
        HooksCommand::Print { target } => {
            print!(
                "{}",
                gate::snippet(target).expect("clap checked the target")
            );
            Ok(Exit::Success)
        }
        HooksCommand::Install | HooksCommand::Uninstall => {
            let repo = Repo::discover(Git::new(std::env::current_dir()?))?;
            if matches!(cmd, HooksCommand::Install) {
                match gate::install(&repo)? {
                    gate::Installed::Written(path) => println!(
                        "Installed {}: every push now checks its commits first (git carve verify --pre-push).",
                        path.display()
                    ),
                    gate::Installed::Already(path) => {
                        println!("Already installed: {}", path.display())
                    }
                }
                println!("For agents: git carve hooks print claude|cursor|codex");
            } else {
                match gate::uninstall(&repo)? {
                    Some(path) => println!("Removed {}.", path.display()),
                    None => println!("No git carve pre-push hook to remove."),
                }
            }
            Ok(Exit::Success)
        }
    }
}

fn run_verify(repo: &Repo, args: &git_carve::cli::VerifyArgs) -> anyhow::Result<Exit> {
    use git_carve::branch::{self, VerifyOptions};
    use std::io::Read;

    let ranges = if args.pre_push {
        let mut stdin = String::new();
        std::io::stdin().read_to_string(&mut stdin)?;
        branch::pre_push_ranges(&stdin, &repo.format.null_oid(), |oid| {
            repo.git
                .test(["cat-file", "-e", &format!("{oid}^{{commit}}")])
                .unwrap_or(false)
        })
    } else if args.range.is_empty() {
        branch::default_ranges(repo)?
    } else {
        vec![args.range.clone()]
    };
    let report = branch::verify_branch(
        repo,
        &VerifyOptions {
            ranges,
            check: args.check.clone(),
            no_new_errors: args.no_new_errors || git_carve::branch::no_new_errors_config(repo)?,
            fail_fast: args.fail_fast || args.pre_push,
        },
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let n = report.commits.len() + report.skipped;
        if n == 0 {
            println!("git carve verify: no commits to check.");
            return Ok(Exit::Success);
        }
        let base = report.baseline_errors.map_or(String::new(), |e| {
            format!(", no new errors beyond the base's {e}")
        });
        println!(
            "git carve verify: {n} commit{}, check: {}{base}",
            if n == 1 { "" } else { "s" },
            report.check
        );
        for c in &report.commits {
            let time = if c.cached {
                "cached".to_owned()
            } else {
                format!("{:.1}s", c.millis as f64 / 1000.0)
            };
            let mark = if c.passed { "ok  " } else { "FAIL" };
            println!(
                "  {mark}  {} {}  ({time})",
                &c.oid[..c.oid.len().min(9)],
                c.subject
            );
            match &c.inherits_from {
                Some(origin) => println!(
                    "        same errors as {}, which introduced them",
                    &origin[..origin.len().min(9)]
                ),
                None => {
                    for e in &c.errors {
                        println!("        {e}");
                    }
                }
            }
        }
        for path in &report.unverified {
            println!("warning: the check does not read {path}; changes to it are unverified");
        }
        match report.failed() {
            0 => println!("Every commit passes."),
            f => {
                if report.skipped > 0 {
                    println!(
                        "A commit does not pass the check. Stopped there: {} later commit{} not checked.",
                        report.skipped,
                        if report.skipped == 1 {
                            " was"
                        } else {
                            "s were"
                        }
                    );
                } else {
                    println!(
                        "{f} of {n} commit{} {} not pass the check.",
                        if n == 1 { "" } else { "s" },
                        if f == 1 { "does" } else { "do" }
                    );
                }
                println!(
                    "hint: to re-split the unpushed commits so each one passes: git carve @{{u}}"
                );
                if let Some(e) = report.base_fails {
                    println!(
                        "hint: the range's base already fails with {e} error{}; to check only for new ones: git carve verify --no-new-errors",
                        if e == 1 { "" } else { "s" }
                    );
                }
            }
        }
    }
    Ok(if report.failed() > 0 {
        Exit::CheckFailed
    } else {
        Exit::Success
    })
}

fn run_eval(cmd: &EvalCommand) -> anyhow::Result<Exit> {
    use git_carve::eval::{corpus, run};
    let env = Git::new(std::env::current_dir()?);
    match cmd {
        EvalCommand::Build {
            sources,
            out,
            train,
            commits,
            max_files,
        } => {
            let text = std::fs::read(sources)?;
            let mut sources: corpus::Sources = serde_json::from_slice(&text)?;
            if let Some((min, max)) = commits {
                (sources.window.min_commits, sources.window.max_commits) = (*min, *max);
            }
            if let Some(max_files) = max_files {
                sources.window.max_files = *max_files;
            }
            // Never overwrite a manifest whose hash eval/LOCK records (D32).
            if let (Ok(existing), Ok(lock)) =
                (std::fs::read(out), std::fs::read_to_string("eval/LOCK"))
            {
                let digest =
                    git_carve::eval::sha256::hex(&git_carve::eval::sha256::sha256(&existing));
                if lock.lines().any(|l| {
                    l.strip_prefix("corpus_manifest_sha256:").map(str::trim)
                        == Some(digest.as_str())
                }) {
                    anyhow::bail!(
                        "{} is the locked EVAL corpus (eval/LOCK); write elsewhere with --out",
                        out.display()
                    );
                }
            }
            let roles: &[corpus::Role] = if *train {
                &[corpus::Role::Train]
            } else {
                &[corpus::Role::EvalUser, corpus::Role::EvalOss]
            };
            let manifest = corpus::build(&sources, roles, &corpus::cache_dir(), &env)?;
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(out, serde_json::to_vec_pretty(&manifest)?)?;
            let tune = manifest
                .windows
                .iter()
                .filter(|w| w.split == corpus::Split::Tune)
                .count();
            println!(
                "Wrote {}: {} windows ({} tune, {} test) from {} sources.",
                out.display(),
                manifest.windows.len(),
                tune,
                manifest.windows.len() - tune,
                manifest.sources.len()
            );
        }
        EvalCommand::Speed {
            corpus: path,
            split,
            jobs,
            warm,
            limit,
            out,
        } => {
            let manifest: corpus::Manifest = serde_json::from_slice(&std::fs::read(path)?)?;
            let split = split.map(|s| match s {
                Split::Tune => corpus::Split::Tune,
                Split::Test => corpus::Split::Test,
            });
            if split == Some(corpus::Split::Test) {
                anyhow::bail!("the speed benchmark never runs on a test split (D51)");
            }
            if jobs.is_empty() {
                anyhow::bail!("--jobs needs at least one count");
            }
            let rows = git_carve::eval::speed::speed(
                &manifest,
                split,
                jobs,
                *warm,
                *limit,
                &corpus::cache_dir(),
                &env,
            )?;
            if let Some(out) = out {
                std::fs::write(out, serde_json::to_string_pretty(&rows)?)?;
            }
            let measured: Vec<_> = rows
                .iter()
                .filter(|r| r.skipped.is_none() && r.final_ms > 0 && r.runs.len() == jobs.len())
                .collect();
            let quantile = |mut v: Vec<f64>, q: f64| {
                v.sort_by(f64::total_cmp);
                v.get(((v.len() as f64 - 1.0) * q).round() as usize)
                    .copied()
                    .unwrap_or(f64::NAN)
            };
            println!(
                "{} windows measured ({} skipped); one cold check of the final tree: median {} ms",
                measured.len(),
                rows.len() - measured.len(),
                quantile(measured.iter().map(|r| r.final_ms as f64).collect(), 0.5)
            );
            for (i, k) in jobs.iter().enumerate() {
                let ratios: Vec<f64> = measured
                    .iter()
                    .map(|r| r.runs[i].wall_ms as f64 / r.final_ms as f64)
                    .collect();
                let diffs = measured
                    .iter()
                    .filter(|r| r.runs[i].plan != r.runs[0].plan)
                    .count();
                println!(
                    "  jobs {k}: wall/one-check median {:.2}, p90 {:.2}; median wall {} ms; plans differing from jobs {}: {diffs}",
                    quantile(ratios.clone(), 0.5),
                    quantile(ratios, 0.9),
                    quantile(
                        measured.iter().map(|r| r.runs[i].wall_ms as f64).collect(),
                        0.5
                    ),
                    jobs[0]
                );
            }
        }
        EvalCommand::RangeCheck {
            corpus: path,
            split,
            limit,
        } => {
            let manifest: corpus::Manifest = serde_json::from_slice(&std::fs::read(path)?)?;
            let split = split.map(|s| match s {
                Split::Tune => corpus::Split::Tune,
                Split::Test => corpus::Split::Test,
            });
            if split == Some(corpus::Split::Test) && path.ends_with("manifest.json") {
                anyhow::bail!("range-check never touches the locked corpus's test split");
            }
            let r = git_carve::eval::range_check::range_check(
                &manifest,
                split,
                *limit,
                &corpus::cache_dir(),
                &env,
            )?;
            let median = |v: &[u64]| {
                let mut v = v.to_vec();
                v.sort_unstable();
                v.get(v.len() / 2).copied().unwrap_or(0)
            };
            println!(
                "{} windows: {} identical, {} expected differences (committed assets kept), {} mismatched, {} refused; median plan ms eval {} vs range {}",
                r.windows,
                r.identical,
                r.expected.len(),
                r.mismatches.len(),
                r.refused.len(),
                median(&r.plan_ms_eval),
                median(&r.plan_ms_range)
            );
            for (id, kept) in &r.expected {
                println!(
                    "expected {id}: range mode keeps {kept} committed new binary or large file(s)"
                );
            }
            for (id, diff) in &r.mismatches {
                println!("MISMATCH {id}: {diff}");
            }
            for (id, reason) in &r.refused {
                println!("refused {id}: {reason}");
            }
            if !r.mismatches.is_empty() {
                return Ok(Exit::CheckFailed);
            }
        }
        EvalCommand::Pairs {
            corpus: path,
            split,
            out,
        } => {
            let manifest: corpus::Manifest = serde_json::from_slice(&std::fs::read(path)?)?;
            let split = split.map(|s| match s {
                Split::Tune => corpus::Split::Tune,
                Split::Test => corpus::Split::Test,
            });
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
            let stats = git_carve::eval::pairs::pairs(
                &manifest,
                split,
                &corpus::cache_dir(),
                &env,
                &mut file,
            )?;
            println!(
                "{} rows from {} windows ({} skipped) -> {}",
                stats.rows,
                stats.windows,
                stats.skipped,
                out.display()
            );
        }
        EvalCommand::Sweep {
            corpus: path,
            split,
            weights,
            linkage,
            thetas,
            out,
        } => {
            let bytes = std::fs::read(path)?;
            let digest = git_carve::eval::sha256::hex(&git_carve::eval::sha256::sha256(&bytes));
            let lock = std::fs::read_to_string("eval/LOCK").unwrap_or_default();
            let locked = lock.lines().any(|l| {
                l.strip_prefix("corpus_manifest_sha256:").map(str::trim) == Some(digest.as_str())
            });
            if locked && *split != Some(Split::Tune) {
                anyhow::bail!("the locked EVAL corpus may be swept on its tune split only (D32)");
            }
            let manifest: corpus::Manifest = serde_json::from_slice(&bytes)?;
            let linkage = match linkage {
                git_carve::cli::Linkage::Average => git_carve::judge::Linkage::Average,
                git_carve::cli::Linkage::Pooled => git_carve::judge::Linkage::Pooled,
            };
            let judges = weights
                .iter()
                .map(|w| -> anyhow::Result<_> {
                    let name = w
                        .file_stem()
                        .map_or("judge".into(), |s| s.to_string_lossy().into_owned());
                    Ok((
                        name,
                        git_carve::judge::LearnedJudge::from_json(
                            &std::fs::read(w)?,
                            0.5,
                            linkage,
                            None,
                        )?,
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let split = split.map(|s| match s {
                Split::Tune => corpus::Split::Tune,
                Split::Test => corpus::Split::Test,
            });
            let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
            let n = git_carve::eval::sweep::sweep(
                &manifest,
                &git_carve::eval::sweep::SweepOptions {
                    split,
                    judges,
                    thetas: thetas.0.clone(),
                },
                &corpus::cache_dir(),
                &env,
                &mut file,
            )?;
            println!("{n} windows swept -> {}", out.display());
        }
        EvalCommand::Capture { note } => {
            let base = corpus::cache_dir().join("real");
            let record =
                git_carve::eval::real::capture(&env, &std::env::current_dir()?, &base, note)?;
            let count = git_carve::eval::real::records(&base)?.len();
            println!(
                "Captured {} ({} in total; stored under {}). Your repository was not modified.",
                &record.id[..12],
                count,
                base.display()
            );
        }
        EvalCommand::RealRender { out, seed } => {
            let base = corpus::cache_dir().join("real");
            let out = out.clone().unwrap_or_else(|| base.join("blind"));
            let skipped = git_carve::eval::real::render(&env, &base, &out, *seed)?;
            for (id, why) in &skipped {
                eprintln!(
                    "skipped {}: {}",
                    &id[..12.min(id.len())],
                    why.lines().next().unwrap_or_default()
                );
            }
            println!(
                "Rendered {} capture(s) into {}. Score each tree's A and B in scores.json, then run `git carve eval real-score`.",
                git_carve::eval::real::records(&base)?.len() - skipped.len(),
                out.display()
            );
        }
        EvalCommand::RealScore { scores, out } => {
            let filled: git_carve::eval::real::Scores =
                serde_json::from_slice(&std::fs::read(scores)?)?;
            let real = git_carve::eval::real::unblind(&filled)?;
            std::fs::write(out, serde_json::to_string_pretty(&real)? + "\n")?;
            println!(
                "Unblinded {} tree(s) -> {}",
                filled.trees.len(),
                out.display()
            );
        }
        EvalCommand::Decide {
            code,
            judge,
            code_tier_b,
            judge_tier_b,
            real,
            out,
        } => {
            let eval_md = std::fs::read_to_string("docs/EVAL.md")?;
            let digest =
                git_carve::eval::sha256::hex(&git_carve::eval::sha256::sha256(eval_md.as_bytes()));
            let lock = std::fs::read_to_string("eval/LOCK")?;
            if !lock
                .lines()
                .any(|l| l.strip_prefix("eval_md_sha256:").map(str::trim) == Some(digest.as_str()))
            {
                anyhow::bail!("docs/EVAL.md does not match the SHA-256 locked in eval/LOCK");
            }
            let read = |p: &std::path::Path| -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::from_slice(
                    &std::fs::read(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?,
                )?)
            };
            let real = real
                .as_deref()
                .map(|p| -> anyhow::Result<_> { Ok(serde_json::from_value(read(p)?)?) })
                .transpose()?;
            let d = git_carve::eval::decide::decide(
                git_carve::eval::decide::locked(&eval_md)?,
                &read(code)?,
                &read(judge)?,
                &read(code_tier_b)?,
                &read(judge_tier_b)?,
                real,
            )?;
            let ci = d
                .e1_gain_ci95
                .map_or("n/a".to_owned(), |(lo, hi)| format!("[{lo:+.3}, {hi:+.3}]"));
            println!(
                "E1 BCubed F1: code {:.3}, judge {:.3}, gain {:+.3} (95% paired CI {ci}, {} windows)",
                d.e1_code, d.e1_judge, d.e1_gain, d.windows_paired
            );
            let opt = |v: Option<f64>| v.map_or("n/a".to_owned(), |v| format!("{v:.2}"));
            println!(
                "E2 merges (tier B, {} windows): code {}, judge {}",
                d.e2_windows_paired,
                opt(d.e2_code),
                opt(d.e2_judge)
            );
            println!(
                "E4 judge precision {:.3}; E5 code {}, judge {}",
                d.e4_precision_judge, d.e5_code, d.e5_judge
            );
            for (guard, holds) in &d.guards {
                let mark = match holds {
                    Some(true) => "holds",
                    Some(false) => "FAILS",
                    None => "pending",
                };
                println!("  {mark:<8} {guard}");
            }
            println!(
                "Verdict: {:?}{}{}",
                d.verdict,
                if d.provisional { " (provisional)" } else { "" },
                if d.launch_blocker {
                    "; E5(code) > 0 is a launch blocker"
                } else {
                    ""
                }
            );
            if let Some(out) = out {
                std::fs::write(out, serde_json::to_string_pretty(&d)? + "\n")?;
                println!("Wrote {}", out.display());
            }
        }
        EvalCommand::Run {
            corpus: path,
            arm,
            weights,
            theta,
            linkage,
            platt,
            split,
            check,
            no_new_errors,
            out,
        } => {
            let judge = match (arm, weights) {
                (Arm::Judge, Some(weights)) => Some(git_carve::judge::LearnedJudge::from_json(
                    &std::fs::read(weights)?,
                    *theta,
                    match linkage {
                        git_carve::cli::Linkage::Average => git_carve::judge::Linkage::Average,
                        git_carve::cli::Linkage::Pooled => git_carve::judge::Linkage::Pooled,
                    },
                    *platt,
                )?),
                (Arm::Judge, None) => anyhow::bail!("the judge arm needs --weights"),
                (Arm::Code, _) => None,
            };
            let bytes = std::fs::read(path)?;
            // D32: the locked corpus's test split sees only the judge frozen in eval/LOCK.
            // Other corpora (the train manifest, D35) are free to use.
            if let Some(judge) = &judge
                && *split != Some(Split::Tune)
            {
                let lock = std::fs::read_to_string("eval/LOCK")?;
                let digest = git_carve::eval::sha256::hex(&git_carve::eval::sha256::sha256(&bytes));
                let locked = lock.lines().any(|l| {
                    l.strip_prefix("corpus_manifest_sha256:").map(str::trim)
                        == Some(digest.as_str())
                });
                if locked {
                    judge.check_frozen(&lock)?;
                }
            }
            let manifest: corpus::Manifest = serde_json::from_slice(&bytes)?;
            let split = split.map(|s| match s {
                Split::Tune => corpus::Split::Tune,
                Split::Test => corpus::Split::Test,
            });
            let results = run::run(
                &manifest,
                &run::RunOptions {
                    split,
                    check: *check,
                    judge,
                    no_new_errors: *no_new_errors,
                },
                &corpus::cache_dir(),
                &env,
            )?;
            let a = &results.aggregate;
            println!(
                "{} windows scored ({} skipped), tier {}, split {}",
                a.windows_scored,
                a.windows_skipped.values().sum::<usize>(),
                results.tier,
                results.split
            );
            let ci = a
                .e1_bcubed_ci95
                .map_or("n/a".to_owned(), |(lo, hi)| format!("[{lo:.3}, {hi:.3}]"));
            println!(
                "E1 BCubed F1 {:.3} (95% CI {ci}); pairwise P {:.3} R {:.3} F1 {:.3}",
                a.e1_bcubed_f1, a.e1_pairwise.precision, a.e1_pairwise.recall, a.e1_pairwise.f1
            );
            println!(
                "E3 group-count error {:.2}; E4 debug P {:.2} R {:.2}; E5 real code left {}; E6 plan {:.0} ms",
                a.e3_group_count_error,
                a.e4_debug.precision,
                a.e4_debug.recall,
                a.e5_real_code_left,
                a.e6_plan_ms_mean
            );
            if let Some(m) = a.e2_merges_mean {
                println!("E2 merges per window {m:.2}");
            }
            if let Some(ari) = a.ari_mean {
                println!("ARI {ari:.3} (mean over windows; not an EVAL.md metric)");
            }
            for (name, s) in &results.per_source {
                println!(
                    "  {name:<22} n={:<4} BCubed F1 {:.3}  pairwise F1 {:.3}  ARI {}",
                    s.windows_scored,
                    s.e1_bcubed_f1,
                    s.e1_pairwise.f1,
                    s.ari_mean.map_or("n/a".to_owned(), |x| format!("{x:.3}"))
                );
            }
            if let Some(out) = out {
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(out, serde_json::to_vec_pretty(&results)?)?;
                println!("Wrote {}", out.display());
            }
        }
    }
    Ok(Exit::Success)
}
