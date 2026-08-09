#![no_main]

use libfuzzer_sys::fuzz_target;
use rsbts::db::Library;
use rsbts::operations::{PlanKind, PlanState};
use serde_json::json;

fuzz_target!(|actions: &[u8]| {
    let Ok(library) = Library::open_in_memory() else {
        return;
    };
    let Ok(id) = library.create_durable_plan(
        PlanKind::Audit,
        &json!({"fuzz": true}),
        &json!({"actions": actions.len()}),
        Some(actions.len() as u64),
    ) else {
        return;
    };
    for (index, action) in actions.iter().take(256).enumerate() {
        match action % 8 {
            0 => {
                let _result = library.approve_durable_plan(&id);
            }
            1 => {
                let _result = library.start_durable_plan(&id);
            }
            2 => {
                let _result = library.pause_durable_plan(&id, Some("cursor"));
            }
            3 => {
                let _result = library.resume_durable_plan(&id);
            }
            4 => {
                let _result = library.update_plan_progress(&id, index as u64, Some("cursor"));
            }
            5 => {
                let _result = library.request_plan_cancellation(&id);
            }
            6 => {
                let _result = library.finish_durable_plan(&id, PlanState::Complete, None);
            }
            _ => {
                let _result = library.finish_durable_plan(&id, PlanState::Failed, Some("fuzz"));
            }
        }
    }
    let _plan = library.durable_plan(&id);
    let _events = library.plan_events(&id);
});
