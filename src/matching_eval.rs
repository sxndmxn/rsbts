//! Reproducible hard-negative evaluation gate for fuzzy exact-release acceptance.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const MIN_HARD_NEGATIVE_CASES: u64 = 30_000;
pub const MIN_RELEASE_GROUP_STRATA: usize = 100;
const MAX_STRATUM_FRACTION: f64 = 0.01;
const Z_95_ONE_SIDED: f64 = 1.644_853_626_951_472_2;

/// One independently identified same-release-group, wrong-edition case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardNegativeCase {
    case_id: String,
    release_group_id: String,
    expected_edition_id: String,
    competing_edition_id: String,
    accepted: bool,
}

impl HardNegativeCase {
    pub fn new(
        case_id: impl Into<String>,
        release_group_id: impl Into<String>,
        expected_edition_id: impl Into<String>,
        competing_edition_id: impl Into<String>,
        accepted: bool,
    ) -> Result<Self> {
        let value = Self {
            case_id: case_id.into(),
            release_group_id: release_group_id.into(),
            expected_edition_id: expected_edition_id.into(),
            competing_edition_id: competing_edition_id.into(),
            accepted,
        };
        for (label, text) in [
            ("case ID", &value.case_id),
            ("release-group ID", &value.release_group_id),
            ("expected edition ID", &value.expected_edition_id),
            ("competing edition ID", &value.competing_edition_id),
        ] {
            if text.trim().is_empty() || text.len() > 512 || text.chars().any(char::is_control) {
                return Err(Error::Operation(format!("invalid hard-negative {label}")));
            }
        }
        if value.expected_edition_id == value.competing_edition_id {
            return Err(Error::Operation(
                "hard-negative editions must be distinct".into(),
            ));
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchingEvaluationReport {
    case_count: u64,
    accepted_count: u64,
    false_accept_count: u64,
    release_group_strata: usize,
    largest_stratum: u64,
    acceptance_coverage: f64,
    false_accept_rate: f64,
    false_accept_rate_upper_95: f64,
    independent_case_ids: bool,
}

impl MatchingEvaluationReport {
    #[must_use]
    pub const fn case_count(&self) -> u64 {
        self.case_count
    }

    #[must_use]
    pub const fn false_accept_count(&self) -> u64 {
        self.false_accept_count
    }

    #[must_use]
    pub const fn acceptance_coverage(&self) -> f64 {
        self.acceptance_coverage
    }

    #[must_use]
    pub const fn false_accept_rate_upper_95(&self) -> f64 {
        self.false_accept_rate_upper_95
    }
}

/// Proof object required by any future unattended fuzzy exact-release path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoacceptAttestation {
    report: MatchingEvaluationReport,
    policy_version: u32,
}

impl AutoacceptAttestation {
    pub fn from_report(report: MatchingEvaluationReport) -> Result<Self> {
        let largest_fraction = report.largest_stratum as f64 / report.case_count.max(1) as f64;
        if report.case_count < MIN_HARD_NEGATIVE_CASES
            || report.false_accept_count != 0
            || !report.independent_case_ids
            || report.release_group_strata < MIN_RELEASE_GROUP_STRATA
            || largest_fraction > MAX_STRATUM_FRACTION
        {
            return Err(Error::Operation(format!(
                "fuzzy exact-release autoaccept gate failed: cases={}, false accepts={}, strata={}, largest stratum {:.2}%",
                report.case_count,
                report.false_accept_count,
                report.release_group_strata,
                largest_fraction * 100.0
            )));
        }
        Ok(Self {
            report,
            policy_version: 1,
        })
    }

    #[must_use]
    pub const fn report(&self) -> &MatchingEvaluationReport {
        &self.report
    }
}

/// Evaluate cases in one pass with memory bounded by case IDs and strata.
pub fn evaluate_hard_negatives(
    cases: impl IntoIterator<Item = HardNegativeCase>,
) -> MatchingEvaluationReport {
    let mut ids = HashSet::new();
    let mut strata = HashMap::<String, u64>::new();
    let mut case_count = 0_u64;
    let mut accepted_count = 0_u64;
    let mut false_accept_count = 0_u64;
    let mut independent = true;
    for case in cases {
        case_count = case_count.saturating_add(1);
        if !ids.insert(case.case_id) {
            independent = false;
        }
        *strata.entry(case.release_group_id).or_default() += 1;
        if case.accepted {
            accepted_count = accepted_count.saturating_add(1);
            // Every case is deliberately the wrong exact edition.
            false_accept_count = false_accept_count.saturating_add(1);
        }
    }
    let coverage = accepted_count as f64 / case_count.max(1) as f64;
    let false_rate = false_accept_count as f64 / case_count.max(1) as f64;
    let upper = wilson_upper_95(false_accept_count, case_count);
    MatchingEvaluationReport {
        case_count,
        accepted_count,
        false_accept_count,
        release_group_strata: strata.len(),
        largest_stratum: strata.values().copied().max().unwrap_or(0),
        acceptance_coverage: coverage,
        false_accept_rate: false_rate,
        false_accept_rate_upper_95: upper,
        independent_case_ids: independent,
    }
}

fn wilson_upper_95(successes: u64, trials: u64) -> f64 {
    if trials == 0 {
        return 1.0;
    }
    let n = trials as f64;
    let proportion = successes as f64 / n;
    let z = Z_95_ONE_SIDED;
    let denominator = 1.0 + z * z / n;
    let center = proportion + z * z / (2.0 * n);
    let radius =
        z * ((proportion * (1.0 - proportion) / n + z * z / (4.0 * n * n)).max(0.0)).sqrt();
    ((center + radius) / denominator).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attestation_requires_30k_independent_balanced_zero_false_accepts() -> Result<()> {
        let cases = (0..MIN_HARD_NEGATIVE_CASES)
            .map(|index| {
                HardNegativeCase::new(
                    format!("case-{index}"),
                    format!("group-{}", index % 300),
                    format!("expected-{index}"),
                    format!("competing-{index}"),
                    false,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let report = evaluate_hard_negatives(cases);
        assert_eq!(report.false_accept_count(), 0);
        assert!(report.acceptance_coverage().abs() < f64::EPSILON);
        assert!(report.false_accept_rate_upper_95() < 0.000_1);
        assert!(AutoacceptAttestation::from_report(report).is_ok());
        Ok(())
    }

    #[test]
    fn one_false_accept_or_duplicate_case_blocks_attestation() -> Result<()> {
        let cases = vec![
            HardNegativeCase::new("same", "group", "a", "b", false)?,
            HardNegativeCase::new("same", "group", "c", "d", true)?,
        ];
        let report = evaluate_hard_negatives(cases);
        assert_eq!(report.false_accept_count(), 1);
        assert!(AutoacceptAttestation::from_report(report).is_err());
        Ok(())
    }
}
