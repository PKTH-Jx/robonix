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
    /// Accumulated admission or cancellation decision for this trigger timing.
    effect: Effect,
    /// Report messages appended in hook execution order.
    messages: Vec<String>,
    /// Requirement and enforcement feedback retained in rule declaration order.
    pub(super) reports: Vec<RuleReport>,
    /// Capability invocations recorded in hook order and completed by the runtime.
    calls: Vec<HookCall>,
    /// Committed exclusion admission to release when this capability finishes executing.
    admission_id: Option<u64>,
}

impl EnforcementBuilder {
    /// Make denial irreversible for this event, including across later allow hooks.
    pub fn deny(&mut self) {
        self.effect = Effect::Deny;
    }

    /// Request cancellation without allowing later hooks to reverse it.
    pub fn stop(&mut self) {
        self.effect = Effect::Stop;
    }

    /// Apply a check failure according to its capability lifecycle timing.
    pub(super) fn fail(&mut self, timing: TriggerTiming) {
        match timing {
            TriggerTiming::Before => self.deny(),
            TriggerTiming::While => self.stop(),
            // After checks report errors without changing the completed execution result.
            TriggerTiming::After => {}
        }
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
    /// Failures deny before admission, stop during execution, and remain feedback afterward.
    pub fn complete_call(
        &mut self,
        index: usize,
        result: Result<(), Error>,
        timing: TriggerTiming,
    ) {
        if let Err(error) = &result {
            self.fail(timing);
            self.reports[self.calls[index].rule].enforcement = Some(Err(error.clone()));
        }
        self.calls[index].result = Some(result);
    }

    /// Return the current decision; pending calls may still cause denial or cancellation.
    pub fn effect(&self) -> Effect {
        self.effect
    }

    /// Preserve check and admission failures alongside the feedback already accumulated.
    pub(in crate::interpreter) fn error(&mut self, error: evaluator::Error, timing: TriggerTiming) {
        self.fail(timing);
        self.reports.push(RuleReport {
            rule: "sentinel".into(),
            evaluation: Err(error),
            enforcement: None,
            timing,
        });
    }

    /// Record exclusion feedback and the admission ID alongside the accumulated rule results.
    pub(in crate::interpreter) fn exclusion(
        &mut self,
        result: Result<Option<u64>, evaluator::Error>,
    ) {
        match result {
            Ok(admission_id) => self.admission_id = admission_id,
            // Exclusions gate dispatch, so their failures always apply before semantics.
            Err(error) => self.error(error, TriggerTiming::Before),
        }
    }

    /// Finalize after recorded calls and exclusion admission have been resolved.
    /// Panics if a recorded call still has no execution outcome.
    pub fn finish(self) -> EnforcementResult {
        assert!(
            self.calls.iter().all(|call| call.result.is_some()),
            "recorded capability calls must complete before finalization"
        );
        EnforcementResult {
            effect: self.effect(),
            messages: self.messages,
            calls: self.calls,
            reports: self.reports,
            admission_id: self.admission_id,
        }
    }
}
