//! Sans-io core of the loop (01, 16). Pure: given what is pending, decide what the driver does next.
//! The driver owns I/O (model calls, tools, the log); every "when do we call the model again"
//! decision is made here so it is testable without a runtime.

use aho_types::CallId;

/// What the loop is waiting on right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    /// Calls issued and not yet resulted.
    pub open_calls: Vec<CallId>,
    /// Results that landed since the last request was sent.
    pub results_since_request: usize,
    /// Operator/system inputs queued since the last request.
    pub inputs: usize,
    /// Of those inputs, how many are urgent (soft-interrupt point C).
    pub urgent: usize,
    /// The last response contained tool calls (grace applies) vs ended the turn.
    pub last_stop_was_tool_use: bool,
    /// A grace window is armed and not yet expired.
    pub grace_armed: bool,
    /// Every call issued by the last response has a result.
    pub graced_all_done: bool,
    /// Steps taken in this turn.
    pub steps: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Build and send the next request now.
    Request,
    /// Keep waiting for a result, an input, grace expiry, or the heartbeat.
    Wait,
    /// Inject a heartbeat notice listing running calls, then request.
    Heartbeat,
    /// Nothing is running and nothing is queued: the turn is over.
    Idle,
    /// The step budget is spent.
    MaxTurns,
}

/// Why the driver is consulting the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Response,
    Result,
    Input,
    GraceExpired,
    HeartbeatDue,
}

/// The whole "should we call the model now" rule set from doc 16:
/// * during grace, only "all graced calls done" or an input ends the window early;
/// * after grace, any landed result or queued input triggers the next request;
/// * a turn that ended without tool calls while calls still run sleeps until one lands;
/// * nothing running and nothing queued is idle; the heartbeat only fires when waiting on calls alone.
pub fn next(p: &Pending, wake: Wake, max_steps: u64) -> Next {
    if p.steps >= max_steps {
        return Next::MaxTurns;
    }
    if p.urgent > 0 || (p.inputs > 0 && !p.grace_armed) {
        return Next::Request;
    }
    if p.grace_armed {
        // during grace only "all graced calls done" or an input ends the window early
        return if p.graced_all_done || p.inputs > 0 { Next::Request } else { Next::Wait };
    }
    match wake {
        Wake::HeartbeatDue if !p.open_calls.is_empty() && p.inputs == 0 && p.results_since_request == 0 => Next::Heartbeat,
        _ if p.results_since_request > 0 => Next::Request,
        _ if p.open_calls.is_empty() && p.inputs == 0 => {
            if p.last_stop_was_tool_use {
                // tool calls all resolved during grace but nothing counted: ask the model to continue
                Next::Request
            } else {
                Next::Idle
            }
        }
        _ => Next::Wait,
    }
}

/// Heartbeat eligibility: only open calls pending, no model call, no inputs (16).
pub fn waiting_on_calls_only(p: &Pending) -> bool {
    !p.open_calls.is_empty() && p.inputs == 0 && p.results_since_request == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> CallId {
        CallId(s.into())
    }

    #[test]
    fn grace_holds_until_all_done_then_requests() {
        let mut p =
            Pending { open_calls: vec![call("a")], last_stop_was_tool_use: true, grace_armed: true, steps: 1, ..Default::default() };
        assert_eq!(next(&p, Wake::Response, 50), Next::Wait);
        p.open_calls.clear();
        p.graced_all_done = true;
        p.results_since_request = 1;
        assert_eq!(next(&p, Wake::Result, 50), Next::Request);
    }

    #[test]
    fn grace_expiry_with_no_result_keeps_waiting_and_a_late_result_wakes() {
        let mut p = Pending { open_calls: vec![call("slow")], last_stop_was_tool_use: true, steps: 1, ..Default::default() };
        assert_eq!(next(&p, Wake::GraceExpired, 50), Next::Wait, "placeholder turn is not forced when nothing landed");
        p.results_since_request = 1;
        p.open_calls.clear();
        assert_eq!(next(&p, Wake::Result, 50), Next::Request);
    }

    #[test]
    fn grace_expiry_with_a_landed_result_requests_even_if_others_run() {
        let p = Pending {
            open_calls: vec![call("slow")],
            results_since_request: 1,
            last_stop_was_tool_use: true,
            steps: 1,
            ..Default::default()
        };
        assert_eq!(next(&p, Wake::GraceExpired, 50), Next::Request);
    }

    #[test]
    fn end_turn_with_calls_running_sleeps_and_without_is_idle() {
        let p = Pending { open_calls: vec![call("bg")], steps: 2, ..Default::default() };
        assert_eq!(next(&p, Wake::Response, 50), Next::Wait);
        let p = Pending { steps: 2, ..Default::default() };
        assert_eq!(next(&p, Wake::Response, 50), Next::Idle);
    }

    #[test]
    fn heartbeat_only_when_waiting_on_calls_alone() {
        let p = Pending { open_calls: vec![call("bg")], steps: 2, ..Default::default() };
        assert_eq!(next(&p, Wake::HeartbeatDue, 50), Next::Heartbeat);
        let p = Pending { open_calls: vec![call("bg")], inputs: 1, steps: 2, ..Default::default() };
        assert_eq!(next(&p, Wake::HeartbeatDue, 50), Next::Request);
    }

    #[test]
    fn urgent_input_breaks_grace_and_step_budget_is_absolute() {
        let p = Pending { open_calls: vec![call("a")], grace_armed: true, urgent: 1, inputs: 1, steps: 3, ..Default::default() };
        assert_eq!(next(&p, Wake::Input, 50), Next::Request);
        let p = Pending { inputs: 1, steps: 50, ..Default::default() };
        assert_eq!(next(&p, Wake::Input, 50), Next::MaxTurns);
    }
}
