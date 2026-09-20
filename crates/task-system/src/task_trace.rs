//! G2.17: Task trace instrumentation for Chrome Trace format.
//!
//! When the `task-trace` feature is enabled, the `#[task]` macro generates
//! instrumentation that logs every task call to a global `SchedulerTrace`.
//!
//! Usage:
//! ```ignore
//! use pledgepack_task_system::task_trace;
//!
//! // Start a trace session
//! task_trace::begin_session();
//!
//! // ... run tasks ...
//!
//! // End the session and get the Chrome Trace JSON
//! let json = task_trace::end_session();
//! // Load in chrome://tracing
//! ```

use std::sync::Mutex;

use crate::engine::SchedulerTrace;

static GLOBAL_TRACE: Mutex<Option<SchedulerTrace>> = Mutex::new(None);

/// Begin a trace session. All subsequent task calls will be recorded.
pub fn begin_session() {
    let mut trace = GLOBAL_TRACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *trace = Some(SchedulerTrace::new());
}

/// End the trace session and return the Chrome Trace JSON.
pub fn end_session() -> String {
    let mut trace = GLOBAL_TRACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(t) = trace.take() {
        t.to_json()
    } else {
        "[]".to_string()
    }
}

/// Record the start of a task computation.
pub fn trace_begin(name: &str) {
    let mut trace = GLOBAL_TRACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(t) = trace.as_mut() {
        t.begin(name, 0, 0);
    }
}

/// Record the end of a task computation.
pub fn trace_end(name: &str) {
    let mut trace = GLOBAL_TRACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(t) = trace.as_mut() {
        t.end(name, 0, 0);
    }
}

/// Record a complete event with a known duration.
pub fn trace_complete(name: &str, dur_micros: u64) {
    let mut trace = GLOBAL_TRACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(t) = trace.as_mut() {
        t.complete(name, 0, 0, dur_micros, None);
    }
}

/// Check if a trace session is active.
pub fn is_tracing() -> bool {
    GLOBAL_TRACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_session_records_events() {
        begin_session();
        assert!(is_tracing());

        trace_begin("test_task");
        trace_end("test_task");

        let json = end_session();
        assert!(!is_tracing());
        assert!(
            json.contains("\"name\":\"test_task\""),
            "JSON should contain task name: {}",
            json
        );
        assert!(json.contains("\"ph\":\"B\""), "Should contain begin event");
        assert!(json.contains("\"ph\":\"E\""), "Should contain end event");
    }

    #[test]
    fn trace_complete_records_duration() {
        begin_session();
        trace_complete("fast_task", 42);
        let json = end_session();
        assert!(
            json.contains("\"dur\":42"),
            "Should contain duration: {}",
            json
        );
    }
}
