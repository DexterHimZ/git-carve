//! `git carve eval decide` (EVAL.md §4 step 6, §5): the pre-registered decision rules, applied
//! mechanically to the arms' results files, so nothing is left to judgment after the test
//! split has been seen (DECISIONS.md D32).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::metrics::{bootstrap_ci, mean};
use crate::error::{Error, Result};

/// The locked values the rules need, read from EVAL.md §6.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Locked {
    pub delta_headline: f64,
    pub delta_optional: f64,
    pub p_debug_min: f64,
    pub bootstrap_seed: u64,
}

pub fn locked(eval_md: &str) -> Result<Locked> {
    let value = |name: &str| -> Result<&str> {
        eval_md
            .lines()
            .find_map(|l| {
                let (key, rest) = l.split_once('=')?;
                (key.trim() == name).then(|| rest.split_whitespace().next().unwrap_or_default())
            })
            .ok_or_else(|| Error::Invalid(format!("EVAL.md: no locked value {name}")))
    };
    let number = |name: &str| -> Result<f64> {
        value(name)?
            .parse()
            .map_err(|_| Error::Invalid(format!("EVAL.md: {name} is not a number")))
    };
    Ok(Locked {
        delta_headline: number("Δ_headline")?,
        delta_optional: number("Δ_optional")?,
        p_debug_min: number("P_debug_min")?,
        bootstrap_seed: value("Bootstrap seed")?
            .parse()
            .map_err(|_| Error::Invalid("EVAL.md: Bootstrap seed is not a number".into()))?,
    })
}

/// E7 inputs from the blinded real-tree scoring (EVAL.md §1.B, step 5).
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct Real {
    pub cuts_code: Vec<f64>,
    pub cuts_judge: Vec<f64>,
    pub preferred_code: usize,
    pub preferred_judge: usize,
    pub ties: usize,
    /// Total leave decisions the user disagrees with (reported; §5 does not gate on it).
    #[serde(default)]
    pub leave_code: f64,
    #[serde(default)]
    pub leave_judge: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// The judge is the headline.
    Headline,
    /// The judge ships as an optional flag, not mentioned in the launch copy.
    Optional,
    /// Ship code-only; the README and launch do not mention the judge.
    CodeOnly,
}

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub locked: Locked,
    pub windows_paired: usize,
    pub e1_code: f64,
    pub e1_judge: f64,
    pub e1_gain: f64,
    pub e1_gain_ci95: Option<(f64, f64)>,
    pub e2_windows_paired: usize,
    pub e2_code: Option<f64>,
    pub e2_judge: Option<f64>,
    pub e4_precision_judge: f64,
    pub e5_code: u64,
    pub e5_judge: u64,
    pub e7: Option<Real>,
    /// Each §5 condition by name, and whether it holds (`None`: not yet measurable).
    pub guards: BTreeMap<&'static str, Option<bool>>,
    pub verdict: Verdict,
    /// True when the verdict could still change: E7 is missing, or fewer than 60 test windows.
    pub provisional: bool,
    /// E5(code) > 0 on test is a launch blocker whatever the verdict.
    pub launch_blocker: bool,
}

/// Per-window values of `field` over windows scored in both results files, in `a`'s order.
fn paired(a: &Value, b: &Value, field: impl Fn(&Value) -> Option<f64>) -> Vec<(f64, f64)> {
    let scored = |r: &Value| -> BTreeMap<String, Value> {
        r["windows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|w| w["skipped"].is_null())
            .filter_map(|w| Some((w["id"].as_str()?.to_owned(), w.clone())))
            .collect()
    };
    let (sa, sb) = (scored(a), scored(b));
    sa.iter()
        .filter_map(|(id, wa)| Some((field(wa)?, field(sb.get(id)?)?)))
        .collect()
}

fn expect(r: &Value, what: &str, arm: &str, tier: &str) -> Result<()> {
    let ok = r["split"] == "test"
        && r["tier"] == tier
        && r["arm"].as_str().is_some_and(|a| a.starts_with(arm));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{what}: expected the {arm} arm on the test split, tier {tier} (found arm {}, split {}, tier {})",
            r["arm"], r["split"], r["tier"]
        )))
    }
}

/// Applies EVAL.md §5 to tier A (`*_a`) and tier B (`*_b`) test results of both arms.
pub fn decide(
    locked: Locked,
    code_a: &Value,
    judge_a: &Value,
    code_b: &Value,
    judge_b: &Value,
    real: Option<Real>,
) -> Result<Decision> {
    expect(code_a, "code tier A", "code", "A")?;
    expect(judge_a, "judge tier A", "judge", "A")?;
    expect(code_b, "code tier B", "code", "B")?;
    expect(judge_b, "judge tier B", "judge", "B")?;

    let e1 = paired(code_a, judge_a, |w| w["bcubed"]["f1"].as_f64());
    let (codes, judges): (Vec<f64>, Vec<f64>) = e1.iter().copied().unzip();
    let diffs: Vec<f64> = e1.iter().map(|(c, j)| j - c).collect();
    let (e1_code, e1_judge) = (mean(&codes), mean(&judges));
    let e1_gain = e1_judge - e1_code;
    let e1_gain_ci95 = bootstrap_ci(&diffs, locked.bootstrap_seed);

    let e2 = paired(code_b, judge_b, |w| w["merges"].as_f64());
    let (m_code, m_judge): (Vec<f64>, Vec<f64>) = e2.iter().copied().unzip();
    let (e2_code, e2_judge) = if e2.is_empty() {
        (None, None)
    } else {
        (Some(mean(&m_code)), Some(mean(&m_judge)))
    };

    let e4_precision_judge = judge_a["aggregate"]["e4_debug"]["precision"]
        .as_f64()
        .unwrap_or(0.0);
    let e5 = |r: &Value| {
        r["aggregate"]["e5_real_code_left"]
            .as_u64()
            .unwrap_or(u64::MAX)
    };
    let (e5_code, e5_judge) = (e5(code_a), e5(judge_a));

    let median = |v: &[f64]| -> f64 {
        let mut v = v.to_vec();
        v.sort_by(f64::total_cmp);
        match v.len() {
            0 => f64::NAN,
            n if n % 2 == 1 => v[n / 2],
            n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
        }
    };
    let ci_above_zero = e1_gain_ci95.is_some_and(|(lo, _)| lo > 0.0);
    let mut guards = BTreeMap::new();
    guards.insert("E1 CI lower bound > 0", Some(ci_above_zero));
    guards.insert(
        "E2 mean(judge) <= mean(code)",
        e2_code.zip(e2_judge).map(|(c, j)| j <= c),
    );
    guards.insert(
        "E4 precision(judge) >= P_debug_min",
        Some(e4_precision_judge >= locked.p_debug_min),
    );
    guards.insert("E5(judge) = 0", Some(e5_judge == 0));
    guards.insert(
        "E7 median cuts(judge) <= median cuts(code), judge preferred >= code",
        real.as_ref().map(|r| {
            median(&r.cuts_judge) <= median(&r.cuts_code) && r.preferred_judge >= r.preferred_code
        }),
    );
    // A guard that fails decides; one not yet measurable keeps the verdict provisional.
    let failed = guards.values().any(|g| *g == Some(false));
    let unknown = guards.values().any(Option::is_none);
    let verdict = if failed {
        Verdict::CodeOnly
    } else if e1_gain >= locked.delta_headline {
        Verdict::Headline
    } else if e1_gain >= locked.delta_optional {
        Verdict::Optional
    } else {
        Verdict::CodeOnly
    };
    Ok(Decision {
        windows_paired: e1.len(),
        e1_code,
        e1_judge,
        e1_gain,
        e1_gain_ci95,
        e2_windows_paired: e2.len(),
        e2_code,
        e2_judge,
        e4_precision_judge,
        e5_code,
        e5_judge,
        e7: real,
        provisional: verdict != Verdict::CodeOnly && (unknown || e1.len() < 60),
        launch_blocker: e5_code > 0,
        guards,
        verdict,
        locked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EVAL: &str = "```\nΔ_headline      = 0.10    # gain\nΔ_optional      = 0.03\nP_debug_min     = 0.95\nBootstrap seed  = 20260924\n```\n";

    fn results(arm: &str, tier: &str, windows: &[(&str, f64, Option<f64>)]) -> Value {
        json!({
            "arm": arm, "split": "test", "tier": tier,
            "aggregate": {"e4_debug": {"precision": 1.0}, "e5_real_code_left": 0},
            "windows": windows.iter().map(|(id, f1, merges)| json!({
                "id": id, "skipped": null, "bcubed": {"f1": f1}, "merges": merges
            })).collect::<Vec<_>>(),
        })
    }

    fn arms(gain: f64, merges: (f64, f64)) -> (Value, Value, Value, Value) {
        let ids: Vec<String> = (0..80).map(|i| format!("w{i}")).collect();
        let w = |base: f64| -> Vec<(&str, f64, Option<f64>)> {
            ids.iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), base + (i % 7) as f64 * 0.01, None))
                .collect()
        };
        let b = |m: f64| -> Vec<(&str, f64, Option<f64>)> {
            ids[..10]
                .iter()
                .map(|id| (id.as_str(), 0.5, Some(m)))
                .collect()
        };
        (
            results("code", "A", &w(0.6)),
            results("judge learned:abc", "A", &w(0.6 + gain)),
            results("code", "B", &b(merges.0)),
            results("judge learned:abc", "B", &b(merges.1)),
        )
    }

    fn real(judge_better: bool) -> Real {
        Real {
            cuts_code: vec![2.0, 3.0],
            cuts_judge: if judge_better {
                vec![1.0, 1.0]
            } else {
                vec![4.0, 5.0]
            },
            preferred_code: 1,
            preferred_judge: if judge_better { 3 } else { 0 },
            ..Real::default()
        }
    }

    #[test]
    fn locked_values_are_read_from_eval_md() {
        let l = locked(EVAL).unwrap();
        assert_eq!(
            (
                l.delta_headline,
                l.delta_optional,
                l.p_debug_min,
                l.bootstrap_seed
            ),
            (0.10, 0.03, 0.95, 20260924)
        );
        assert!(locked("nothing here").is_err());
    }

    #[test]
    fn rules_follow_eval_section_5() {
        let l = locked(EVAL).unwrap();
        let run = |gain: f64, merges: (f64, f64), real: Option<Real>| {
            let (ca, ja, cb, jb) = arms(gain, merges);
            decide(l.clone(), &ca, &ja, &cb, &jb, real).unwrap()
        };
        let d = run(0.12, (1.0, 0.5), Some(real(true)));
        assert_eq!((d.verdict, d.provisional), (Verdict::Headline, false));
        assert_eq!(d.windows_paired, 80);
        let d = run(0.05, (1.0, 0.5), Some(real(true)));
        assert_eq!(d.verdict, Verdict::Optional);
        let d = run(0.01, (1.0, 0.5), Some(real(true)));
        assert_eq!(d.verdict, Verdict::CodeOnly);
        // A failed guard sends a large gain to code-only.
        assert_eq!(
            run(0.2, (0.5, 1.0), Some(real(true))).verdict,
            Verdict::CodeOnly
        );
        assert_eq!(
            run(0.2, (1.0, 0.5), Some(real(false))).verdict,
            Verdict::CodeOnly
        );
        // Missing E7 keeps a winning verdict provisional.
        let d = run(0.12, (1.0, 0.5), None);
        assert_eq!((d.verdict, d.provisional), (Verdict::Headline, true));
    }

    #[test]
    fn wrong_files_are_refused() {
        let l = locked(EVAL).unwrap();
        let (ca, ja, cb, jb) = arms(0.1, (1.0, 1.0));
        assert!(decide(l.clone(), &ja, &ca, &cb, &jb, None).is_err());
        assert!(decide(l, &ca, &ja, &ca, &jb, None).is_err());
    }
}
