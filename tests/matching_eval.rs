use rsbts::matching_eval::{
    evaluate_hard_negatives, AutoacceptAttestation, HardNegativeCase, MatchingEvaluationReport,
    MIN_HARD_NEGATIVE_CASES,
};
use serde_json::{json, Value};

fn evaluation(
    count: u64,
    groups: u64,
    accepted: bool,
    duplicate: bool,
) -> rsbts::Result<MatchingEvaluationReport> {
    let cases = (0..count)
        .map(|index| {
            HardNegativeCase::new(
                format!("case-{}", if duplicate && index == 1 { 0 } else { index }),
                format!("group-{}", index % groups),
                "expected",
                "competing",
                accepted && index == 0,
            )
        })
        .collect::<rsbts::Result<Vec<_>>>()?;
    Ok(evaluate_hard_negatives(cases))
}

#[test]
fn case_json_enforces_constructor_rules() -> Result<(), Box<dyn std::error::Error>> {
    let case = HardNegativeCase::new("case", "group", "expected", "competing", false)?;
    let value = serde_json::to_value(&case)?;
    let restored: HardNegativeCase = serde_json::from_str(&serde_json::to_string(&case)?)?;
    assert_eq!(serde_json::to_value(restored)?, value);
    let mut accepted = Vec::new();
    for field in [
        "case_id",
        "release_group_id",
        "expected_edition_id",
        "competing_edition_id",
    ] {
        for invalid in [String::new(), " ".into(), "x".repeat(513), "x\n".into()] {
            let mut changed = value.clone();
            changed[field] = json!(invalid);
            if serde_json::from_value::<HardNegativeCase>(changed).is_ok() {
                accepted.push(format!("{field}: {invalid:?}"));
            }
        }
    }
    let mut identical = value;
    identical["competing_edition_id"] = identical["expected_edition_id"].clone();
    if serde_json::from_value::<HardNegativeCase>(identical).is_ok() {
        accepted.push("identical editions".into());
    }
    assert!(accepted.is_empty(), "accepted invalid cases: {accepted:?}");
    Ok(())
}

#[test]
fn report_json_rejects_impossible_aggregates_and_rates() -> Result<(), Box<dyn std::error::Error>> {
    for count in [0, 1, 3, 17, MIN_HARD_NEGATIVE_CASES] {
        for accepted in [false, true] {
            let report = evaluation(count, 300, accepted, false)?;
            let restored: MatchingEvaluationReport =
                serde_json::from_str(&serde_json::to_string(&report)?)?;
            let before = serde_json::to_value(report)?;
            let mut after = serde_json::to_value(restored)?;
            for field in [
                "acceptance_coverage",
                "false_accept_rate",
                "false_accept_rate_upper_95",
            ] {
                let expected = before[field].as_f64().ok_or("missing expected rate")?;
                let actual = after[field].as_f64().ok_or("missing restored rate")?;
                assert!((actual - expected).abs() <= expected.abs() * f64::EPSILON * 4.0);
                after[field] = before[field].clone();
            }
            assert_eq!(after, before);
        }
    }
    let value = serde_json::to_value(evaluation(6, 3, true, false)?)?;
    let invalid = [
        json!({"case_count": 0}),
        json!({"accepted_count": 7, "false_accept_count": 7}),
        json!({"false_accept_count": 0}),
        json!({"release_group_strata": 0}),
        json!({"release_group_strata": 7}),
        json!({"largest_stratum": 0}),
        json!({"largest_stratum": 7}),
        json!({"largest_stratum": 1}),
        json!({"largest_stratum": 5}),
        json!({"acceptance_coverage": 0.123}),
        json!({"false_accept_rate": 0.123}),
        json!({"false_accept_rate_upper_95": 0.123}),
    ];
    let mut accepted = Vec::new();
    for changes in invalid {
        let mut changed = value.clone();
        for (field, replacement) in changes.as_object().ok_or("missing changes")? {
            changed[field] = replacement.clone();
        }
        if serde_json::from_value::<MatchingEvaluationReport>(changed).is_ok() {
            accepted.push(changes);
        }
    }
    let mut impossible_duplicate = serde_json::to_value(evaluation(1, 1, false, false)?)?;
    impossible_duplicate["independent_case_ids"] = json!(false);
    if serde_json::from_value::<MatchingEvaluationReport>(impossible_duplicate).is_ok() {
        accepted.push(json!({"one_case_independence": false}));
    }
    assert!(
        accepted.is_empty(),
        "accepted invalid reports: {accepted:?}"
    );
    Ok(())
}

#[test]
fn attestation_json_enforces_policy_and_evaluation_gate() -> Result<(), Box<dyn std::error::Error>>
{
    let attestation = AutoacceptAttestation::from_report(evaluation(
        MIN_HARD_NEGATIVE_CASES,
        300,
        false,
        false,
    )?)?;
    let value = serde_json::to_value(&attestation)?;
    let restored: AutoacceptAttestation =
        serde_json::from_str(&serde_json::to_string(&attestation)?)?;
    assert_eq!(serde_json::to_value(restored)?, value);
    let mut accepted = Vec::new();
    for version in [0, 2, u32::MAX] {
        let mut changed = value.clone();
        changed["policy_version"] = json!(version);
        if serde_json::from_value::<AutoacceptAttestation>(changed).is_ok() {
            accepted.push(format!("policy version {version}"));
        }
    }
    for (label, count, groups, false_accept, duplicate) in [
        (
            "too few cases",
            MIN_HARD_NEGATIVE_CASES - 1,
            300,
            false,
            false,
        ),
        ("false accept", MIN_HARD_NEGATIVE_CASES, 300, true, false),
        ("duplicate case", MIN_HARD_NEGATIVE_CASES, 300, false, true),
        (
            "concentrated groups",
            MIN_HARD_NEGATIVE_CASES,
            99,
            false,
            false,
        ),
    ] {
        let report = evaluation(count, groups, false_accept, duplicate)?;
        assert!(AutoacceptAttestation::from_report(report.clone()).is_err());
        let changed: Value = json!({"report": report, "policy_version": 1});
        if serde_json::from_value::<AutoacceptAttestation>(changed).is_ok() {
            accepted.push(label.into());
        }
    }
    assert!(
        accepted.is_empty(),
        "accepted invalid attestations: {accepted:?}"
    );
    Ok(())
}
