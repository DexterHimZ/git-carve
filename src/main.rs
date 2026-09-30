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
    let repo = Repo::discover(Git::new(std::env::current_dir()?))?;
    let _lock = repo.lock()?;

    if cli.run.undo {
        let record = undo::undo(&repo)?;
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

    let opts = Options {
        check: cli.run.check.clone(),
        no_check: cli.run.no_check,
        plan_file: cli.run.plan_file.clone(),
        judge: cli.run.judge.then(git_carve::judge::LearnedJudge::builtin),
        no_new_errors: cli.run.no_new_errors,
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
        if cli.run.json {
            print!(
                "{}",
                report::json(
                    &prepared.changes,
                    &prepared.analysis,
                    &prepared.plan,
                    prepared.verification.as_ref()
                )
            );
        } else {
            let messages = vec![None; prepared.plan.groups.len()];
            print!(
                "{}",
                plan::render(
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
                )
            );
            if let Some(v) = &prepared.verification {
                eprintln!(
                    "({} check runs, {} cached, {:.1}s)",
                    v.runs,
                    v.cached,
                    v.millis as f64 / 1000.0
                );
            }
        }
        return Ok(Exit::Success);
    }

    let summary = session::run(&repo, &opts)?;
    print_summary(&summary);
    Ok(Exit::Success)
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
    match &s.verification {
        Some(v) => {
            let runs = match v.cached {
                0 => format!("{} check runs", v.runs),
                c => format!("{} check runs, {c} cached", v.runs),
            };
            let passed = match v.baseline_errors {
                Some(e) => format!(
                    "each adds no error beyond the {e} HEAD already had under `{}`",
                    v.check
                ),
                None => format!("each passed `{}`", v.check),
            };
            println!(
                "Created {n} commit{plural} on {place}, {passed} ({runs}, {} merge{}, {:.1}s):",
                s.merges,
                if s.merges == 1 { "" } else { "s" },
                v.millis as f64 / 1000.0
            )
        }
        None => {
            println!("Created {n} commit{plural} on {place}. They are NOT verified (--no-check):")
        }
    }
    for (oid, subject) in s.record.commits.iter().zip(&s.subjects) {
        println!("  {} {subject}", &oid[..7]);
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
            for (name, s) in &results.per_source {
                println!(
                    "  {name:<22} n={:<4} BCubed F1 {:.3}  pairwise F1 {:.3}",
                    s.windows_scored, s.e1_bcubed_f1, s.e1_pairwise.f1
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
