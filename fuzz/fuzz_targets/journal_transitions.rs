#![no_main]

use libfuzzer_sys::fuzz_target;
use rsbts::db::Library;
use rsbts::operations::{PlanKind, PlanState};
use serde_json::json;

fuzz_target!(|actions: &[u8]| {
    let library = Library::open_in_memory().expect("generated catalog must open");
    let id = library
        .create_durable_plan(
            PlanKind::Audit,
            &json!({"fuzz": true}),
            &json!({"actions": actions.len()}),
            Some(actions.len() as u64),
        )
        .expect("generated plan must be stored");
    let transitions = [
        (PlanState::Planned, PlanState::Approved),
        (PlanState::Approved, PlanState::Running),
        (PlanState::Running, PlanState::Paused),
        (PlanState::Paused, PlanState::Running),
    ];
    for (index, action) in actions.iter().take(256).enumerate() {
        let action = usize::from(action % 8);
        let before = library
            .durable_plan(&id)
            .expect("saved plan must be readable");
        let result = match action {
            0 => library.approve_durable_plan(&id),
            1 => library.start_durable_plan(&id),
            2 => library.pause_durable_plan(&id, Some("cursor")),
            3 => library.resume_durable_plan(&id),
            4 => library.update_plan_progress(&id, index as u64, Some("cursor")),
            5 => library.request_plan_cancellation(&id),
            6 => library.finish_durable_plan(&id, PlanState::Complete, None),
            _ => library.finish_durable_plan(&id, PlanState::Failed, Some("fuzz")),
        };
        let after = library
            .durable_plan(&id)
            .expect("saved plan must remain readable");
        if let Some((from, to)) = transitions.get(action) {
            let allowed = before.state() == *from && !before.cancel_requested();
            assert_eq!(result.is_ok(), allowed, "action {action} from {before:?}");
            if allowed {
                assert_eq!(after.state(), *to, "action {action}");
            }
        }
        if result.is_err() {
            assert_eq!(
                serde_json::to_value(&after).expect("saved plan must serialize"),
                serde_json::to_value(&before).expect("saved plan must serialize"),
                "failed action {action} changed the saved plan"
            );
        }
    }
    let events = library
        .plan_events(&id)
        .expect("saved events must be readable");
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(
            event.sequence(),
            sequence as u64,
            "event sequence must be contiguous"
        );
    }
});
