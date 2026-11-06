//! Command-line surface (SPEC §11). Parsing only; no behavior lives here.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Splits your mess into commits that each pass your check.
/// Refuses to write your commit messages.
#[derive(Debug, Parser)]
#[command(
    name = "git-carve",
    bin_name = "git carve",
    version,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(flatten)]
    pub run: RunArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Re-carve everything since BASE: the commits in BASE..HEAD plus your uncommitted
    /// changes (for example `main`, `HEAD~3`, or `@{u}` for everything unpushed)
    #[arg(value_name = "BASE")]
    pub base: Option<String>,

    /// Allow re-carving commits that are already on a remote (you will need to force-push)
    #[arg(long, requires = "base")]
    pub rewrite_pushed: bool,

    /// Also make each new commit a branch: a stack of PRs for `gh stack`, every layer
    /// already checked. Prints the commands to submit it; pushes nothing
    #[arg(long, requires = "base")]
    pub stack: bool,

    /// With --stack: at most this many layers; the smallest neighbors merge until the stack
    /// fits (each merge keeps every layer passing). 0 for no limit [default: 5]
    #[arg(long, value_name = "N", requires = "stack")]
    pub layers: Option<usize>,

    /// After committing, submit the stack with `gh stack` (pushes the layers, opens draft PRs)
    #[arg(long, requires = "stack", conflicts_with_all = ["no_check", "plan_only"])]
    pub submit: bool,

    /// Check command to run on every commit (overrides `carve.check` and the default)
    #[arg(long, value_name = "CMD")]
    pub check: Option<String>,

    /// Skip verification; the summary will say the commits are unverified
    #[arg(long, conflicts_with = "check")]
    pub no_check: bool,

    /// Refuse when HEAD already fails the check. By default carve tolerates HEAD's errors and
    /// commits only what adds no new one (D53)
    #[arg(long, conflicts_with = "no_check")]
    pub strict: bool,

    /// The default since D53; still accepted so older scripts and hooks keep working
    #[arg(long, hide = true, conflicts_with_all = ["no_check", "strict"])]
    pub no_new_errors: bool,

    /// Code-only grouping (the default)
    #[cfg(feature = "eval")]
    #[arg(long)]
    pub no_judge: bool,

    /// Let the built-in learned judge decide merges code cannot settle (local, deterministic)
    #[cfg(feature = "eval")]
    #[arg(long, conflicts_with = "no_judge")]
    pub judge: bool,

    /// Compute and verify, print the plan to stdout, commit nothing
    #[arg(long)]
    pub plan_only: bool,

    /// Read a filled plan buffer from this file instead of opening the editor; its message
    /// lines become the commit messages, and the run records that they came from a file. With
    /// --edit, the editor opens on it first. `carve.requireHumanMessages` refuses filled
    /// messages without --edit (DECISIONS.md D58)
    #[arg(long, value_name = "PATH", conflicts_with = "plan_only")]
    pub plan_file: Option<PathBuf>,

    /// With --plan-file: open the editor on the plan file, to review it and write the messages
    #[arg(long, requires = "plan_file")]
    pub edit: bool,

    /// Print the plan and metrics as JSON
    #[arg(long)]
    pub json: bool,

    /// Print check commands, timings, and judge decisions
    #[arg(short, long)]
    pub verbose: bool,

    /// Undo the last carve
    #[arg(long, exclusive = true)]
    pub undo: bool,

    /// Remove the scratch worktree, caches, and the build target dir
    #[arg(long, exclusive = true)]
    pub clean: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check that every commit in a range passes the check (pre-push or CI)
    Verify(VerifyArgs),
    /// Gate pushes on the check: git's pre-push hook, and agents' pre-tool hooks
    #[command(subcommand)]
    Hooks(HooksCommand),
    /// The evaluation harness (docs/EVAL.md): build the corpus, run an arm
    #[cfg(feature = "eval")]
    #[command(hide = true, subcommand)]
    Eval(EvalCommand),
}

#[derive(Debug, Subcommand)]
pub enum HooksCommand {
    /// Write the pre-push hook that runs `git carve verify --pre-push` (never overwrites
    /// another hook)
    Install,
    /// Remove git carve's pre-push hook
    Uninstall,
    /// Print the hook setup for an agent or git; carve never writes agent config files
    Print {
        #[arg(value_parser = ["claude", "cursor", "codex", "git"])]
        target: String,
    },
    /// Run as an agent's pre-tool hook: read the event on stdin, deny a `git push` whose
    /// commits fail the check
    Run {
        #[arg(value_parser = ["claude", "cursor", "codex"])]
        agent: String,
    },
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Commits to check, as git revision arguments (default: commits not yet on the upstream,
    /// or on no remote)
    #[arg(value_name = "RANGE")]
    pub range: Vec<String>,

    /// Check command (overrides `carve.check` and the default)
    #[arg(long, value_name = "CMD")]
    pub check: Option<String>,

    /// Also fail commits for errors the range's base already has. By default a commit fails
    /// only for new ones (D53)
    #[arg(long)]
    pub strict: bool,

    /// The default since D53; still accepted so older scripts and hooks keep working
    #[arg(long, hide = true, conflicts_with = "strict")]
    pub no_new_errors: bool,

    /// Read the refs being pushed from a pre-push hook's standard input (implies --fail-fast)
    #[arg(long, conflicts_with = "range")]
    pub pre_push: bool,

    /// Stop at the first failing commit: a verdict, not a full report
    #[arg(long)]
    pub fail_fast: bool,

    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    /// Clone the sources and write the window manifest
    Build {
        #[arg(long, default_value = "eval/sources.json")]
        sources: PathBuf,
        #[arg(long, default_value = "eval/corpus/manifest.json")]
        out: PathBuf,
        /// Build from training sources instead of eval sources (DECISIONS.md D21)
        #[arg(long)]
        train: bool,
        /// Override the sources file's commits per window as MIN-MAX, for training corpora only
        /// (DECISIONS.md D38); the EVAL corpus is 2-4
        #[arg(long, requires = "train", value_parser = parse_range)]
        commits: Option<(usize, usize)>,
        /// Override the sources file's file cap per window, for training corpora only
        #[arg(long, requires = "train")]
        max_files: Option<usize>,
    },
    /// Proves `git carve <base>` plans exactly like the evaluated path, window by window
    /// (D46 phase 1). Code-only; never the locked corpus's test split
    #[command(hide = true)]
    RangeCheck {
        #[arg(long, default_value = "eval/corpus/train-manifest-v2.json")]
        corpus: PathBuf,
        #[arg(long, value_enum)]
        split: Option<Split>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// D51's pre-registered speed benchmark: tier-B windows carved cold at each job count;
    /// the verify step's wall time over one cold check of the final tree, and plan
    /// differences between job counts. Never the locked corpus's test split
    #[command(hide = true)]
    Speed {
        #[arg(long, default_value = "eval/corpus/manifest.json")]
        corpus: PathBuf,
        #[arg(long, value_enum)]
        split: Option<Split>,
        /// Job counts to compare, the first being the reference for plan differences
        #[arg(long, value_delimiter = ',', default_value = "1,2,3")]
        jobs: Vec<usize>,
        /// Steady state: every worker's build info starts at HEAD (default: cold)
        #[arg(long)]
        warm: bool,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Write labeled component-pair features for the learned judge (DECISIONS.md D28)
    Pairs {
        #[arg(long, default_value = "eval/corpus/manifest.json")]
        corpus: PathBuf,
        #[arg(long, value_enum)]
        split: Option<Split>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Window-level BCubed of several judges at many thresholds, one analysis per window
    /// (DECISIONS.md D38). Refuses the locked corpus's test split
    Sweep {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, value_enum)]
        split: Option<Split>,
        /// Weights files; each judge is named by its file stem
        #[arg(long, num_args = 1.., required = true)]
        weights: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = Linkage::Pooled)]
        linkage: Linkage,
        /// Thresholds as START:STOP:STEP (inclusive)
        #[arg(long, default_value = "0.30:0.90:0.05", value_parser = parse_thetas)]
        thetas: Thetas,
        #[arg(long)]
        out: PathBuf,
    },
    /// Save the current messy working tree for the blinded real-tree scoring (EVAL.md §1.B,
    /// DECISIONS.md D36). Reads this repository; writes only under the eval cache
    Capture {
        /// What you were doing, for your own reference when scoring
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Render every capture's two plans, blinded as A and B, plus a scores.json template
    /// (EVAL.md step 5). The output holds your code: keep it outside the project
    RealRender {
        /// Default: the eval cache's real/blind directory
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 20260924)]
        seed: u64,
    },
    /// Unblind a filled-in scores.json into E7's inputs for `eval decide --real`
    RealScore {
        #[arg(long)]
        scores: PathBuf,
        #[arg(long, default_value = "eval/results/real.json")]
        out: PathBuf,
    },
    /// Apply EVAL.md §5's decision rules to both arms' test results (DECISIONS.md D32)
    Decide {
        #[arg(long, default_value = "eval/results/baseline-test.json")]
        code: PathBuf,
        #[arg(long, default_value = "eval/results/judge-test.json")]
        judge: PathBuf,
        #[arg(long, default_value = "eval/results/baseline-tierb-test.json")]
        code_tier_b: PathBuf,
        #[arg(long, default_value = "eval/results/judge-tierb-test.json")]
        judge_tier_b: PathBuf,
        /// E7 from the blinded real-tree scoring; the verdict is provisional without it
        #[arg(long)]
        real: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run an arm over the corpus and write results
    Run {
        #[arg(long, default_value = "eval/corpus/manifest.json")]
        corpus: PathBuf,
        #[arg(long, value_enum, default_value_t = Arm::Code)]
        arm: Arm,
        /// Judge arm: the learned judge's weights file (DECISIONS.md D28)
        #[arg(long, required_if_eq("arm", "judge"))]
        weights: Option<PathBuf>,
        /// Judge arm: merge while the best pair's probability is at least this
        #[arg(long, default_value_t = 0.5)]
        theta: f64,
        /// Judge arm: how two clusters of components are scored
        #[arg(long, value_enum, default_value_t = Linkage::Average)]
        linkage: Linkage,
        /// Judge arm: Platt scaling `A,B` fitted on the tune split (EVAL.md §4 step 3)
        #[arg(long, value_parser = parse_platt)]
        platt: Option<(f64, f64)>,
        #[arg(long, value_enum)]
        split: Option<Split>,
        /// Tier B: run the check, so merge-forward applies (DECISIONS.md D23)
        #[arg(long)]
        check: bool,
        /// Tier B: tolerate errors HEAD already has (DECISIONS.md D43)
        #[arg(long, requires = "check")]
        no_new_errors: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Linkage {
    Average,
    Pooled,
}

/// A threshold grid for `eval sweep`.
#[derive(Clone, Debug)]
pub struct Thetas(pub Vec<f64>);

fn parse_thetas(s: &str) -> Result<Thetas, String> {
    let parts: Vec<f64> = s
        .split(':')
        .map(|p| p.trim().parse::<f64>().map_err(|e| format!("{e}")))
        .collect::<Result<_, _>>()?;
    let [start, stop, step] = parts[..] else {
        return Err("expected START:STOP:STEP".into());
    };
    if step <= 0.0 || stop < start {
        return Err("expected START <= STOP and STEP > 0".into());
    }
    let n = ((stop - start) / step + 1e-9).floor() as usize;
    Ok(Thetas(
        (0..=n)
            .map(|i| ((start + step * i as f64) * 1e6).round() / 1e6)
            .collect(),
    ))
}

fn parse_range(s: &str) -> Result<(usize, usize), String> {
    let (a, b) = s.split_once('-').ok_or("expected MIN-MAX")?;
    let (a, b): (usize, usize) = (
        a.trim().parse().map_err(|e| format!("{e}"))?,
        b.trim().parse().map_err(|e| format!("{e}"))?,
    );
    if a < 1 || b < a {
        return Err("expected 1 <= MIN <= MAX".into());
    }
    Ok((a, b))
}

fn parse_platt(s: &str) -> Result<(f64, f64), String> {
    let (a, b) = s.split_once(',').ok_or("expected A,B")?;
    Ok((
        a.trim().parse().map_err(|e| format!("{e}"))?,
        b.trim().parse().map_err(|e| format!("{e}"))?,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Arm {
    Code,
    Judge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Split {
    Tune,
    Test,
}
