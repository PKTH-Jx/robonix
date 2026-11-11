// SPDX-License-Identifier: MulanPSL-2.0

//! One owner for active rule checks, acquisitions, and FIFO evaluation.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
};

use futures::{
    FutureExt, StreamExt,
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
};
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

use super::{CallKey, Check, Error, Evaluation, RULE_TIMEOUT, RuleEvaluator, RuleResult};
use crate::{
    interpreter::{
        adapter::{Deadline, InputAdapter, Prepared},
        trigger_matcher::TriggerMatcher,
    },
    srdl::ast::Program,
};

const MAX_FRESHNESS_RETRIES: usize = 2;
type RuleKey = (CallKey, usize);

/// Acquisition cancellation and freshness retained until a matching rule finishes.
struct RuleEntry {
    /// Cancels acquisition while preserving the Adapter's connection cleanup.
    acquisition: Option<AbortHandle>,
    /// Retained deadlines keep early results valid through combined enforcement.
    freshness: Option<Deadline>,
    /// Bounds reacquisition of the active clause group when its snapshot expires.
    retries: usize,
}

/// Metadata and combined results for one active lifecycle check.
struct Invocation {
    /// Distinguishes late responses from a later reuse of the same call key.
    check_id: u64,
    /// Atlas identity used for acquiring rule inputs.
    consumer: String,
    /// Trusted arguments and identity for this check.
    context: Arc<Value>,
    /// Source indices of rules selected for this check.
    rules: Vec<usize>,
    /// Completed results ordered by their rule's source index.
    results: BTreeMap<usize, Result<RuleResult, Error>>,
    /// Delivers this check's requirement results exactly once.
    reply: oneshot::Sender<Result<Evaluation, Error>>,
    /// Detects owner drop before a queued cancellation notification is processed.
    owner: watch::Receiver<()>,
    /// Retires the cancellation watcher together with the invocation.
    cancellation: AbortHandle,
    /// Fixed budget for the current condition group, preserved across retries.
    deadline: Instant,
}

/// A completed acquisition waiting for its atomic evaluation turn.
struct ReadyCheck {
    /// Identifies the rule within its invocation.
    key: RuleKey,
    /// Rejects events left over from an earlier invocation.
    check_id: u64,
    /// A fixed observation set, or the failure that prevented acquisition.
    data: Result<Prepared, Error>,
}

/// Single owner of pool mutations; acquisition futures run independently of FIFO readiness.
pub(super) struct Pool<'a> {
    /// Policy that remains immutable for the worker's lifetime.
    program: &'a Program,
    /// Shared Adapter used by independently polled acquisition streams.
    adapter: &'a InputAdapter,
    /// Active rule entries keyed by plan, call, and source rule index.
    rules: HashMap<RuleKey, RuleEntry>,
    /// Combined results and cancellation metadata for active checks.
    calls: HashMap<CallKey, Invocation>,
    /// Acquisition completions, each carrying its original invocation generation.
    acquiring: FuturesUnordered<BoxFuture<'a, Option<ReadyCheck>>>,
    /// Owner-drop notifications that cannot be lost to a full command channel.
    cancelled: FuturesUnordered<BoxFuture<'a, Option<(CallKey, u64)>>>,
    /// Complete snapshots scheduled in arrival order.
    ready: VecDeque<ReadyCheck>,
}

impl<'a> Pool<'a> {
    pub(super) fn new(program: &'a Program, adapter: &'a InputAdapter) -> Self {
        Self {
            program,
            adapter,
            rules: HashMap::new(),
            calls: HashMap::new(),
            acquiring: FuturesUnordered::new(),
            cancelled: FuturesUnordered::new(),
            ready: VecDeque::new(),
        }
    }

    /// Multiplex work without holding a lock across acquisition or completion waits.
    /// Cancellation has a separate stream so a full command queue cannot prevent cleanup.
    pub(super) async fn run(mut self, mut checks: mpsc::Receiver<Check>) {
        loop {
            tokio::select! {
                // Process check requests in arrival order
                check = checks.recv() => match check {
                    Some(check) => self.check(check),
                    None => break,
                },
                // Process acquisition completions
                Some(result) = self.acquiring.next(), if !self.acquiring.is_empty() => {
                    if let Some(result) = result
                        && self.current(&result.key.0, result.check_id)
                        && let Some(entry) = self.rules.get_mut(&result.key) {
                                entry.acquisition = None;
                                self.ready.push_back(result);
                    }
                },
                // Process cancellation notifications
                Some(cancelled) = self.cancelled.next(), if !self.cancelled.is_empty() => {
                    if let Some((key, check_id)) = cancelled
                        && self.current(&key, check_id) { self.remove(&key); }
                },
                // Evaluate ready snapshots
                _ = std::future::ready(()), if !self.ready.is_empty() => {
                    let ready = self.ready.pop_front().unwrap();
                    self.evaluate(ready);
                },
            }
        }
        // Abort handles do not cancel on drop; dropping the futures also cancels input streams.
    }

    /// Returns true if the call key is still active and its check ID matches the current invocation.
    fn current(&self, key: &CallKey, check_id: u64) -> bool {
        self.calls
            .get(key)
            .is_some_and(|call| call.check_id == check_id)
    }

    /// Register either timing through the same matching and acquisition path.
    fn check(&mut self, check: Check) {
        let Check {
            key,
            check_id,
            capability,
            consumer,
            context,
            cancel,
            reply,
            permit,
        } = check;
        let rules = TriggerMatcher::match_indices(self.program, &capability, key.2);
        super::super::logging::log(
            robonix_scribe::Level::Info,
            format!(
                "[sentinel] check_id={check_id} plan_id={:?} call_id={:?} timing={:?} capability={capability:?} consumer={consumer:?} triggered_rules={:?}",
                key.0,
                key.1,
                key.2,
                rules
                    .iter()
                    .map(|index| self.program.rules[*index].name.0.as_str())
                    .collect::<Vec<_>>()
            ),
        );
        if rules.is_empty() {
            let _ = reply.send(Ok(Evaluation {
                check_id,
                rules: BTreeMap::new(),
            }));
            return;
        }
        if self
            .calls
            .get(&key)
            .is_some_and(|call| call.owner.has_changed().is_err())
        {
            self.remove(&key);
        }
        if self.calls.contains_key(&key) {
            let _ = reply.send(Err(Error::Pool(
                "Capability event already has active Sentinel checks",
            )));
            return;
        }
        let (cancellation, registration) = AbortHandle::new_pair();
        self.calls.insert(
            key.clone(),
            Invocation {
                check_id,
                consumer,
                context,
                rules: rules.clone(),
                results: BTreeMap::new(),
                reply,
                owner: cancel.clone(),
                cancellation,
                deadline: Instant::now() + RULE_TIMEOUT,
            },
        );
        let cancelled_key = key.clone();
        self.cancelled.push(
            async move {
                // Capacity is released only when even an aborted watcher is retired.
                let _permit = permit;
                Abortable::new(
                    async move {
                        let mut cancel = cancel;
                        let _ = cancel.changed().await;
                        (cancelled_key, check_id)
                    },
                    registration,
                )
                .await
                .ok()
            }
            .boxed(),
        );
        self.acquire(&key, rules);
    }

    /// Register peers together before fetching, then retain one atomic snapshot per rule.
    fn acquire(&mut self, key: &CallKey, indices: Vec<usize>) {
        let call = &self.calls[key];
        let check_id = call.check_id;
        let deadline = call.deadline;
        let datasets = self.adapter.acquire_group(
            indices.iter().map(|index| &self.program.rules[*index]),
            &call.consumer,
            call.context.clone(),
            check_id,
            deadline,
        );
        for (index, dataset) in indices.into_iter().zip(datasets) {
            let key = (key.clone(), index);
            let (abort, registration) = AbortHandle::new_pair();
            let entry = self.rules.entry(key.clone()).or_insert(RuleEntry {
                acquisition: None,
                freshness: None,
                retries: 0,
            });
            entry.freshness = None;
            entry.acquisition = Some(abort);
            self.acquiring.push(
                async move {
                    Abortable::new(
                        async move {
                            let data = dataset.await;
                            ReadyCheck {
                                key,
                                check_id,
                                data,
                            }
                        },
                        registration,
                    )
                    .await
                    .ok()
                }
                .boxed(),
            );
        }
    }

    /// Only expiry can retry; the original timeout and attempt limit remain authoritative.
    fn retry(&mut self, key: &RuleKey, result: &Result<RuleResult, Error>) -> bool {
        if !matches!(result, Err(Error::Expired { .. }))
            || Instant::now() >= self.calls[&key.0].deadline
        {
            return false;
        }
        let entry = self.rules.get_mut(key).unwrap();
        if entry.retries >= MAX_FRESHNESS_RETRIES {
            return false;
        }
        entry.retries += 1;
        super::super::logging::log(
            robonix_scribe::Level::Info,
            format!(
                "[sentinel] check_id={} rule={:?} freshness_retry={}",
                self.calls[&key.0].check_id, self.program.rules[key.1].name.0, entry.retries,
            ),
        );
        true
    }

    /// Run one complete clause group without yielding between its predicates.
    fn evaluate(&mut self, ready: ReadyCheck) {
        if !self.current(&ready.key.0, ready.check_id) {
            return;
        }
        let Some(entry) = self.rules.get_mut(&ready.key) else {
            return;
        };
        let mut result = match ready.data {
            Ok(prepared) => {
                let rule = &self.program.rules[ready.key.1];
                let result = prepared
                    .deadline
                    .check(rule, Instant::now())
                    .and_then(|()| {
                        let result = RuleEvaluator::evaluate(ready.check_id, rule, &prepared)?;
                        prepared.deadline.check(rule, Instant::now())?;
                        Ok(result)
                    });
                entry.freshness = Some(prepared.deadline);
                result
            }
            Err(error) => Err(error),
        };
        if Instant::now() >= self.calls[&ready.key.0].deadline {
            result = Err(Error::Timeout {
                rule: self.program.rules[ready.key.1].name.0.clone(),
            });
        }
        if self.retry(&ready.key, &result) {
            self.acquire(&ready.key.0, vec![ready.key.1]);
            return;
        }
        let call = self.calls.get_mut(&ready.key.0).unwrap();
        call.results.insert(ready.key.1, result);
        if call.results.len() == call.rules.len() {
            self.finish_check(&ready.key.0);
        }
    }

    /// Recheck freshness before returning combined results or scheduling retries.
    fn finish_check(&mut self, key: &CallKey) {
        let results = std::mem::take(&mut self.calls.get_mut(key).unwrap().results);
        let mut completed = BTreeMap::new();
        let mut retries = Vec::new();
        for (index, result) in results {
            let passed = result.as_ref().is_ok_and(RuleResult::passed);
            let entry = self.rules.get_mut(&(key.clone(), index)).unwrap();
            let result = result.and_then(|result| {
                if let Some(freshness) = &entry.freshness {
                    freshness.check(&self.program.rules[index], tokio::time::Instant::now())?;
                }
                Ok(result)
            });
            if passed && self.retry(&(key.clone(), index), &result) {
                retries.push(index);
            } else {
                completed.insert(index, result);
            }
        }
        let call = self.calls.get_mut(key).unwrap();
        if !retries.is_empty() {
            call.results = completed;
            self.acquire(key, retries);
            return;
        }
        let check_id = call.check_id;
        let _ = self.remove(key).reply.send(Ok(Evaluation {
            check_id,
            rules: completed,
        }));
    }

    /// Abort acquisition and retire the watcher even if its caller never polls the reply.
    fn remove(&mut self, key: &CallKey) -> Invocation {
        let call = self.calls.remove(key).unwrap();
        call.cancellation.abort();
        for index in &call.rules {
            if let Some(entry) = self.rules.remove(&(key.clone(), *index))
                && let Some(acquisition) = entry.acquisition
            {
                acquisition.abort();
            }
        }
        self.ready
            .retain(|ready| &ready.key.0 != key || ready.check_id != call.check_id);
        call
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
