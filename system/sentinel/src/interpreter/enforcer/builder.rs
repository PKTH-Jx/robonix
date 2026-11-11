use super::{Effect, EnforcementResult, Error, RuleReport};
use crate::interpreter::evaluator;
use crate::srdl::ast::TriggerTiming;

/// A capability invocation recorded by a hook for execution by the runtime.
#[derive(Debug)]
pub struct HookCall {
    /// Contract ID to resolve and invoke with empty arguments.
    pub capability: String,
    /// Index of the originating rule in this event's feedback.
    pub rule: usize,
    /// Execution outcome, absent until the runtime has handled this call.
    pub result: Option<Result<(), Error>>,
}

/// Accumulates hook decisions and feedback for one capability event.
#[derive(Default)]
pub struct EnforcementBuilder {
    /// A denial survives all later hooks, including allow hooks.
    denied: bool,
    /// Report messages appended in hook execution order.
    messages: Vec<String>,
    /// Requirement and enforcement feedback retained in rule declaration order.
    pub(super) reports: Vec<RuleReport>,
    /// Capability invocations recorded in hook order and completed by the runtime.
    calls: Vec<HookCall>,
}

impl EnforcementBuilder {
    /// Make denial irreversible for this event, including across later allow hooks.
    pub fn deny(&mut self) {
        self.denied = true;
    }

    /// Retain each message separately so repeated report hooks keep their order.
    pub fn report(&mut self, message: impl Into<String>) {
        self.messages.push(message.into());
    }

    /// Queue a call for the rule currently being built, before its report is appended.
    pub fn call(&mut self, capability: impl Into<String>) {
        self.calls.push(HookCall {
            capability: capability.into(),
            rule: self.reports.len(),
            result: None,
        });
    }

    pub fn calls(&self) -> &[HookCall] {
        &self.calls
    }

    /// Attach a runtime outcome to the originating rule after its report has been appended.
    /// Failures deny before admission and remain feedback for after checks.
    pub fn complete_call(
        &mut self,
        index: usize,
        result: Result<(), Error>,
        timing: TriggerTiming,
    ) {
        if let Err(error) = &result {
            if timing == TriggerTiming::Before {
                self.deny();
            }
            self.reports[self.calls[index].rule].enforcement = Some(Err(error.clone()));
        }
        self.calls[index].result = Some(result);
    }

    /// Return the current decision; executing pending calls may still add denial.
    pub fn effect(&self) -> Effect {
        if self.denied {
            Effect::Deny
        } else {
            Effect::Allow
        }
    }

    /// Preserve check and admission failures alongside the feedback already accumulated.
    pub(in crate::interpreter) fn error(&mut self, error: evaluator::Error, timing: TriggerTiming) {
        if timing == TriggerTiming::Before {
            self.deny();
        }
        self.reports.push(RuleReport {
            rule: "sentinel".into(),
            evaluation: Err(error),
            enforcement: None,
            timing,
        });
    }

    /// Finalize after recorded calls and exclusion admission have been resolved.
    /// Panics if a recorded call still has no execution outcome.
    pub fn finish(self, admission_id: Option<u64>) -> EnforcementResult {
        assert!(
            self.calls.iter().all(|call| call.result.is_some()),
            "recorded capability calls must complete before finalization"
        );
        EnforcementResult {
            effect: self.effect(),
            messages: self.messages,
            calls: self.calls,
            reports: self.reports,
            admission_id,
        }
    }
}
