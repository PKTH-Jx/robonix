// SPDX-License-Identifier: MulanPSL-2.0

//! Capability exclusions owned independently of rule evaluation.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Mutex,
};

use super::evaluator::Error;
use crate::srdl::ast::ExcludeClause;

const CAPACITY: usize = 1024;
type CallKey = (String, String);

/// Restriction ownership for one call undergoing admission or executing.
struct Call {
    /// Generation required when releasing this admission.
    id: u64,
    /// Contract used in directed relation lookups.
    capability: String,
    /// Permission has been issued and survives cancellation of the check future.
    committed: bool,
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
pub(super) struct ExclusionProcessor {
    /// Mutual declarations expand into two edges; duplicate declarations collapse here.
    relations: BTreeSet<(String, String)>,
    /// Only short admission and completion operations hold this lock.
    state: Mutex<State>,
}

/// Cancels pending admission on drop without releasing already issued permission.
pub(super) struct Admission<'a> {
    /// Processor borrowed for pending-admission cleanup.
    processor: &'a ExclusionProcessor,
    /// Executor's plan and call identity.
    key: CallKey,
    /// Distinguishes this attempt from later uses of the same identity.
    id: u64,
}

impl ExclusionProcessor {
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

    /// Check if a source capability excludes a target capability
    fn excludes(&self, source: &str, target: &str) -> bool {
        self.relations
            .contains(&(source.to_owned(), target.to_owned()))
    }

    /// Pending conflicts invalidate both calls; committed calls apply only outgoing edges.
    pub(super) fn begin(
        &self,
        plan_id: &str,
        call_id: &str,
        capability: &str,
    ) -> Result<Option<Admission<'_>>, Error> {
        if self.relations.is_empty() {
            return Ok(None);
        }
        let key = (plan_id.to_owned(), call_id.to_owned());
        let mut state = self.state.lock().unwrap();
        if state.calls.contains_key(&key) {
            return Err(Error::Pool(
                "Capability call already has an exclusion admission",
            ));
        }
        let mut conflict = None;
        for call in state.calls.values_mut() {
            if self.excludes(&call.capability, capability)
                || (!call.committed && self.excludes(capability, &call.capability))
            {
                conflict = Some(call.capability.clone());
                if !call.committed {
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
            return Err(Error::Exclusion {
                capability: capability.to_owned(),
                conflicting,
            });
        }
        if state.calls.len() >= CAPACITY {
            return Err(Error::Pool(
                "Sentinel exclusion admission capacity exhausted",
            ));
        }
        state.next_id += 1;
        let id = state.next_id;
        state.calls.insert(
            key.clone(),
            Call {
                id,
                capability: capability.to_owned(),
                committed: false,
                conflict: None,
            },
        );
        Ok(Some(Admission {
            processor: self,
            key,
            id,
        }))
    }

    /// Completion must refer to the admission that actually executed, not a reused call ID.
    pub(super) fn finish(&self, plan_id: &str, call_id: &str, id: u64) {
        let key = (plan_id.to_owned(), call_id.to_owned());
        let mut state = self.state.lock().unwrap();
        if state
            .calls
            .get(&key)
            .is_some_and(|call| call.id == id && call.committed)
        {
            state.calls.remove(&key);
        }
    }
}

impl Admission<'_> {
    /// Recheck and record permission atomically after asynchronous rule checking.
    pub(super) fn enforce(&self, allow: bool) -> Result<Option<u64>, Error> {
        let mut state = self.processor.state.lock().unwrap();
        let call = &state.calls[&self.key];
        let conflicting = call.conflict.clone();
        if let Some(conflicting) = conflicting {
            super::logging::log(
                robonix_scribe::Level::Warn,
                format!(
                    "[sentinel] exclusion denied plan_id={:?} call_id={:?} capability={:?} conflicting={:?}",
                    self.key.0, self.key.1, call.capability, conflicting
                ),
            );
            return Err(Error::Exclusion {
                capability: call.capability.clone(),
                conflicting,
            });
        }
        if !allow {
            return Ok(None);
        }
        state.calls.get_mut(&self.key).unwrap().committed = true;
        Ok(Some(self.id))
    }
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let mut state = self.processor.state.lock().unwrap();
        if state
            .calls
            .get(&self.key)
            .is_some_and(|call| call.id == self.id && !call.committed)
        {
            state.calls.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_exclusions_follow_admission_order_and_retain_all_owners() {
        let processor = ExclusionProcessor::new(
            &crate::srdl::parse("exclude A B; exclude A B;")
                .unwrap()
                .excludes,
        );
        assert_eq!(processor.relations.len(), 1);
        let b = processor.begin("p", "b", "B").unwrap().unwrap();
        let b_id = b.enforce(true).unwrap().unwrap();
        let a1 = processor.begin("p", "a1", "A").unwrap().unwrap();
        let id1 = a1.enforce(true).unwrap().unwrap();
        let a2 = processor.begin("q", "a2", "A").unwrap().unwrap();
        let id2 = a2.enforce(true).unwrap().unwrap();
        assert!(matches!(
            processor.begin("q", "b", "B"),
            Err(Error::Exclusion { .. })
        ));
        drop((a1, a2));
        processor.finish("p", "a1", id1);
        assert!(processor.begin("q", "b", "B").is_err());
        processor.finish("q", "a2", id2);
        assert!(processor.begin("q", "b", "B").is_ok());
        processor.finish("p", "b", b_id);
        assert!(processor.state.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn mutual_pending_conflicts_reject_both_and_cancelled_checks_leave_no_admission() {
        let processor =
            ExclusionProcessor::new(&crate::srdl::parse("exclude mutual A B;").unwrap().excludes);
        for (source, target) in [("A", "B"), ("B", "A")] {
            let pending = processor.begin("p", "a", source).unwrap().unwrap();
            assert!(matches!(
                processor.begin("q", "b", target),
                Err(Error::Exclusion { .. })
            ));
            assert!(matches!(
                pending.enforce(true),
                Err(Error::Exclusion { .. })
            ));
            drop(pending);
            let cancelled = processor.begin("p", "a", source).unwrap().unwrap();
            drop(cancelled);
            let admitted = processor.begin("q", "b", target).unwrap().unwrap();
            let old = admitted.enforce(true).unwrap().unwrap();
            drop(admitted);
            assert!(processor.begin("p", "a", source).is_err());
            processor.finish("q", "b", old);
            let reused = processor.begin("q", "b", target).unwrap().unwrap();
            let current = reused.enforce(true).unwrap().unwrap();
            processor.finish("q", "b", old);
            assert!(processor.begin("p", "a", source).is_err());
            processor.finish("q", "b", current);
        }
    }

    #[test]
    fn bounds_execution_state_until_confirmed_completion() {
        let processor =
            ExclusionProcessor::new(&crate::srdl::parse("exclude A B;").unwrap().excludes);
        let mut last = 0;
        for index in 0..CAPACITY {
            let admission = processor
                .begin("p", &index.to_string(), "A")
                .unwrap()
                .unwrap();
            last = admission.enforce(true).unwrap().unwrap();
        }
        assert!(matches!(
            processor.begin("q", "extra", "A"),
            Err(Error::Pool(_))
        ));
        processor.finish("p", &(CAPACITY - 1).to_string(), last);
        assert!(processor.begin("q", "extra", "A").is_ok());
    }
}
