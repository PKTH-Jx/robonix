// SPDX-License-Identifier: MulanPSL-2.0

//! Capability exclusions owned independently of rule evaluation.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, Weak},
};

use super::{
    enforcer::{Effect, EnforcementBuilder},
    evaluator::Error,
};
use crate::srdl::ast::ExcludeClause;

const CAPACITY: usize = 1024;
type CallKey = (String, String);

/// Restriction ownership for one call undergoing admission or executing.
struct Call {
    /// Generation required when releasing this admission.
    id: u64,
    /// Executor joins this task if the call or a nested corrective call panics.
    task: Option<tokio::task::Id>,
    /// Contract used in directed relation lookups.
    capability: String,
    /// A pending check holds its admission ID until it commits. Admitted calls wait for completion.
    pending: Option<Weak<u64>>,
    /// A conflicting arrival invalidated this pending admission.
    conflict: Option<String>,
}

/// Mutable admission state serialized across concurrent before checks.
#[derive(Default)]
struct State {
    /// Pending and committed calls, bounded by CAPACITY.
    calls: HashMap<CallKey, Call>,
    /// Separates completion notifications from later reuse of a call identity.
    next_id: u64,
}

/// Normalized directed restrictions and the calls currently owning them.
pub(super) struct ExclusionChecker {
    /// Mutual declarations expand into two edges; duplicate declarations collapse here.
    relations: BTreeSet<(String, String)>,
    /// Only short admission and completion operations hold this lock.
    state: Mutex<State>,
}

impl ExclusionChecker {
    pub(super) fn new(clauses: &[ExcludeClause]) -> Self {
        let mut relations = BTreeSet::new();
        for clause in clauses {
            relations.insert((clause.source.clone(), clause.target.clone()));
            if clause.mutual {
                relations.insert((clause.target.clone(), clause.source.clone()));
            }
        }
        Self {
            relations,
            state: Mutex::new(State::default()),
        }
    }

    fn excludes(&self, source: &str, target: &str) -> bool {
        self.relations
            .contains(&(source.to_owned(), target.to_owned()))
    }

    /// Reserve a call before rule evaluation, then recheck conflicts at commit.
    /// Conflicting pending checks invalidate each other before either may commit.
    pub(super) fn begin(
        &self,
        plan_id: &str,
        call_id: &str,
        capability: &str,
        builder: &mut EnforcementBuilder,
    ) -> Option<Arc<u64>> {
        if self.relations.is_empty() {
            return None;
        }
        let key = (plan_id.to_owned(), call_id.to_owned());
        // Keep conflict detection and reservation atomic across concurrent checks.
        let mut state = self.state.lock().unwrap();
        // A discarded arrival can never dispatch. Reclaim it before checking conflicts or capacity.
        state
            .calls
            .retain(|_, call| call.pending.as_ref().is_none_or(|id| id.strong_count() > 0));
        if state.calls.contains_key(&key) {
            builder.exclusion(Err(Error::Pool(
                "Capability call already has an exclusion admission",
            )));
            return None;
        }
        let mut conflict = None;
        // Committed calls apply outgoing exclusions. Pending calls check both directions
        // so finishing rule evaluation first cannot let either conflicting call proceed.
        for call in state.calls.values_mut() {
            if self.excludes(&call.capability, capability)
                || (call.pending.is_some() && self.excludes(capability, &call.capability))
            {
                conflict = Some(call.capability.clone());
                if call.pending.is_some() {
                    // Mark every pending conflict; its commit must fail when it resumes.
                    call.conflict = Some(capability.to_owned());
                }
            }
        }
        if let Some(conflicting) = conflict {
            super::logging::log(
                robonix_scribe::Level::Warn,
                format!(
                    "[sentinel] exclusion denied plan_id={plan_id:?} call_id={call_id:?} capability={capability:?} conflicting={conflicting:?}"
                ),
            );
            builder.exclusion(Err(Error::Exclusion {
                capability: capability.to_owned(),
                conflicting,
            }));
            return None;
        }
        // Bound both pending checks and admitted executions retained by this checker.
        if state.calls.len() >= CAPACITY {
            builder.exclusion(Err(Error::Pool(
                "Sentinel exclusion admission capacity exhausted",
            )));
            return None;
        }
        // Distinguish this reservation from later reuse of the same plan and call IDs.
        state.next_id += 1;
        let id = Arc::new(state.next_id);
        state.calls.insert(
            key,
            Call {
                id: *id,
                task: tokio::task::try_id(),
                capability: capability.to_owned(),
                pending: Some(Arc::downgrade(&id)),
                conflict: None,
            },
        );
        Some(id)
    }

    /// Commit permission or explicitly discard the pending reservation on denial.
    pub(super) fn commit(
        &self,
        plan_id: &str,
        call_id: &str,
        id: Arc<u64>,
        builder: &mut EnforcementBuilder,
    ) {
        let key = (plan_id.to_owned(), call_id.to_owned());
        let mut state = self.state.lock().unwrap();
        let call = &state.calls[&key];
        assert_eq!(call.id, *id, "commit must refer to the reserved admission");
        // Another arrival may have invalidated this reservation during rule or hook execution.
        if let Some(conflicting) = call.conflict.clone() {
            super::logging::log(
                robonix_scribe::Level::Warn,
                format!(
                    "[sentinel] exclusion denied plan_id={plan_id:?} call_id={call_id:?} capability={:?} conflicting={conflicting:?}",
                    call.capability
                ),
            );
            builder.exclusion(Err(Error::Exclusion {
                capability: call.capability.clone(),
                conflicting,
            }));
        }
        if builder.effect() == Effect::Allow {
            state.calls.get_mut(&key).unwrap().pending = None;
            builder.exclusion(Ok(Some(*id)));
        } else {
            // No permission escaped to execution, so completion will have nothing to release.
            state.calls.remove(&key);
        }
    }

    /// Completion must refer to the admission that actually executed, not a reused call ID.
    pub(super) fn finish(&self, plan_id: &str, call_id: &str, id: u64) {
        let key = (plan_id.to_owned(), call_id.to_owned());
        let mut state = self.state.lock().unwrap();
        if state
            .calls
            .get(&key)
            .is_some_and(|call| call.id == id && call.pending.is_none())
        {
            state.calls.remove(&key);
        }
    }

    pub(super) fn finish_task(&self, plan_id: &str, task: tokio::task::Id) {
        self.state
            .lock()
            .unwrap()
            .calls
            .retain(|(plan, _), call| plan != plan_id || call.task != Some(task));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::enforcer::EnforcementResult;

    fn arrive(
        processor: &ExclusionChecker,
        plan: &str,
        call: &str,
        cap: &str,
    ) -> EnforcementResult {
        let mut builder = EnforcementBuilder::default();
        if let Some(id) = processor.begin(plan, call, cap, &mut builder) {
            processor.commit(plan, call, id, &mut builder);
        }
        builder.finish()
    }

    #[test]
    fn directed_exclusions_follow_admission_order_and_retain_all_owners() {
        let processor = ExclusionChecker::new(
            &crate::srdl::parse("exclude A B; exclude A B;")
                .unwrap()
                .excludes,
        );
        assert_eq!(processor.relations.len(), 1);
        let b = arrive(&processor, "p", "b", "B").admission_id.unwrap();
        let a1 = arrive(&processor, "p", "a1", "A").admission_id.unwrap();
        let a2 = arrive(&processor, "q", "a2", "A").admission_id.unwrap();
        assert_eq!(arrive(&processor, "q", "b", "B").effect, Effect::Deny);
        processor.finish("p", "a1", a1);
        assert_eq!(arrive(&processor, "q", "b", "B").effect, Effect::Deny);
        processor.finish("q", "a2", a2);
        let next = arrive(&processor, "q", "b", "B");
        assert_eq!(next.effect, Effect::Allow);
        processor.finish("q", "b", next.admission_id.unwrap());
        processor.finish("p", "b", b);
        assert!(processor.state.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn pending_conflicts_and_denials_release_reservations_explicitly() {
        let processor =
            ExclusionChecker::new(&crate::srdl::parse("exclude mutual A B;").unwrap().excludes);
        for (source, target) in [("A", "B"), ("B", "A")] {
            let mut builder = EnforcementBuilder::default();
            let pending = processor.begin("p", "a", source, &mut builder).unwrap();
            assert_eq!(arrive(&processor, "q", "b", target).effect, Effect::Deny);
            processor.commit("p", "a", pending, &mut builder);
            let denied = builder.finish();
            assert_eq!(denied.effect, Effect::Deny);
            assert!(denied.admission_id.is_none());
            assert!(matches!(
                denied.reports[0].evaluation,
                Err(Error::Exclusion { .. })
            ));

            let mut cancelled = EnforcementBuilder::default();
            let pending = processor.begin("p", "a", source, &mut cancelled).unwrap();
            cancelled.deny();
            processor.commit("p", "a", pending, &mut cancelled);
            assert!(cancelled.finish().admission_id.is_none());
            let old = arrive(&processor, "q", "b", target).admission_id.unwrap();
            assert_eq!(arrive(&processor, "p", "a", source).effect, Effect::Deny);
            processor.finish("q", "b", old);
            let current = arrive(&processor, "q", "b", target).admission_id.unwrap();
            processor.finish("q", "b", old);
            assert_eq!(arrive(&processor, "p", "a", source).effect, Effect::Deny);
            processor.finish("q", "b", current);
        }
        assert!(processor.state.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn bounds_execution_state_until_confirmed_completion() {
        let processor =
            ExclusionChecker::new(&crate::srdl::parse("exclude A B;").unwrap().excludes);
        let mut last = 0;
        for index in 0..CAPACITY {
            last = arrive(&processor, "p", &index.to_string(), "A")
                .admission_id
                .unwrap();
        }
        let denied = arrive(&processor, "q", "extra", "A");
        assert_eq!(denied.effect, Effect::Deny);
        assert!(matches!(denied.reports[0].evaluation, Err(Error::Pool(_))));
        processor.finish("p", &(CAPACITY - 1).to_string(), last);
        let mut builder = EnforcementBuilder::default();
        let abandoned = processor.begin("p", "pending", "A", &mut builder).unwrap();
        assert_eq!(arrive(&processor, "q", "extra", "A").effect, Effect::Deny);
        drop(abandoned);
        assert_eq!(arrive(&processor, "q", "extra", "A").effect, Effect::Allow);
    }

    #[tokio::test]
    async fn failed_task_releases_its_call_and_corrective_call() {
        let clauses = crate::srdl::parse("exclude A X; exclude B Y; exclude C Z;").unwrap();
        let checker = Arc::new(ExclusionChecker::new(&clauses.excludes));
        let sibling = arrive(&checker, "plan", "sibling", "C")
            .admission_id
            .unwrap();
        let failed = tokio::spawn({
            let checker = checker.clone();
            async move {
                assert_eq!(arrive(&checker, "plan", "call", "A").effect, Effect::Allow);
                assert_eq!(arrive(&checker, "plan", "hook", "B").effect, Effect::Allow);
                panic!("call task panicked after admission");
            }
        })
        .await
        .unwrap_err();
        assert!(failed.is_panic());
        assert_eq!(arrive(&checker, "other", "x", "X").effect, Effect::Deny);
        assert_eq!(arrive(&checker, "other", "y", "Y").effect, Effect::Deny);
        checker.finish_task("plan", failed.id());
        assert_eq!(arrive(&checker, "other", "x", "X").effect, Effect::Allow);
        assert_eq!(arrive(&checker, "other", "y", "Y").effect, Effect::Allow);
        assert_eq!(arrive(&checker, "other", "z", "Z").effect, Effect::Deny);
        checker.finish("plan", "sibling", sibling);
        assert_eq!(arrive(&checker, "other", "z", "Z").effect, Effect::Allow);
    }
}
