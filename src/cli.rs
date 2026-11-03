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
    /// Check command to run on every commit (overrides `carve.check` and the default)
    #[arg(long, value_name = "CMD")]
    pub check: Option<String>,

    /// Skip verification; the summary will say the commits are unverified
    #[arg(long, conflicts_with = "check")]
    pub no_check: bool,

    /// When HEAD already fails the check, commit only what adds no new error to HEAD's
    #[arg(long, conflicts_with = "no_check")]
    pub no_new_errors: bool,

    /// Code-only grouping (the default)
    #[arg(long)]
    pub no_judge: bool,

    /// Let the built-in learned judge decide merges code cannot settle (local, deterministic)
    #[arg(long, conflicts_with = "no_judge")]
    pub judge: bool,

    /// Compute and verify, print the plan to stdout, commit nothing
    #[arg(long)]
    pub plan_only: bool,

    /// Read a filled plan buffer from this file instead of opening the editor
    #[arg(long, value_name = "PATH", conflicts_with = "plan_only")]
    pub plan_file: Option<PathBuf>,

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
    /// The evaluation harness (docs/EVAL.md): build the corpus, run an arm
    #[command(hide = true, subcommand)]
    Eval(EvalCommand),
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

    /// Tolerate errors the range's base already has: a commit fails only for new ones
    #[arg(long)]
    pub no_new_errors: bool,

    /// Read the refs being pushed from a pre-push hook's standard input
    #[arg(long, conflicts_with = "range")]
    pub pre_push: bool,

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
