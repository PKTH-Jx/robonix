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

use super::{CallKey, Check, Error, RULE_TIMEOUT, RuleEvaluator, RuleResult};
use crate::{
    interpreter::{
        adapter::{Deadline, InputAdapter, Prepared},
        enforcer::{Effect, EnforcementResult, Enforcer},
        trigger_matcher::TriggerMatcher,
    },
    srdl::ast::Program,
};

const MAX_FRESHNESS_RETRIES: usize = 2;
type RuleKey = (CallKey, usize);

/// Processing state of one rule's preconditions or postconditions.
#[derive(Clone, Copy)]
enum Status {
    Waiting,
    Evaluating,
    Enforcing,
}

/// Processing state retained for a matching rule until its applicable checks finish.
struct RuleEntry {
    /// Current stage of this independently triggered check.
    status: Status,
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
    consumer: Arc<str>,
    /// Trusted arguments and identity for this check.
    context: Arc<Value>,
    /// Source indices of rules selected for this check.
    rules: Vec<usize>,
    /// Completed results ordered by their rule's source index.
    results: BTreeMap<usize, Result<RuleResult, Error>>,
    /// Delivers this check's combined decision and reports exactly once.
    reply: oneshot::Sender<EnforcementResult>,
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
                                entry.status = Status::Evaluating;
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
            let decision = EnforcementResult {
                admission_id: None,
                effect: Effect::Allow,
                reports: Vec::new(),
            };
            super::super::SrdlInterpreter::log_decision(check_id, &decision);
            let _ = reply.send(decision);
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
            let _ = reply.send(EnforcementResult::failure(
                "Capability event already has active Sentinel checks",
                key.2,
            ));
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
                status: Status::Waiting,
                acquisition: None,
                freshness: None,
                retries: 0,
            });
            entry.status = Status::Waiting;
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
        let entry = self.rules.get_mut(&ready.key).unwrap();
        entry.status = Status::Enforcing;
        let call = self.calls.get_mut(&ready.key.0).unwrap();
        call.results.insert(ready.key.1, result);
        if call.results.len() == call.rules.len() {
            self.enforce(&ready.key.0);
        }
    }

    /// Combine every matched rule, rechecking snapshots that finished before slower rules.
    fn enforce(&mut self, key: &CallKey) {
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
        let decision = Enforcer::enforce(
            completed
                .into_iter()
                .map(|(index, result)| (&self.program.rules[index], result)),
            key.2,
        );
        super::super::SrdlInterpreter::log_decision(call.check_id, &decision);
        let _ = self.remove(key).reply.send(decision);
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
mod tests {
    use super::*;
    use crate::srdl::ast::TriggerTiming;
    use robonix_atlas::{
        client::AtlasClient,
        service::{AtlasRegistry, serve_atlas},
    };
    use tokio::{
        sync::Semaphore,
        time::{Duration, timeout},
    };

    async fn adapter() -> (InputAdapter, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(serve_atlas(Arc::new(AtlasRegistry::default()), address));
        let atlas = AtlasClient::connect_with_retry(
            format!("http://{address}"),
            50,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        (InputAdapter::new(atlas), server)
    }

    fn snapshot(rule: &crate::srdl::ast::Rule, overall: Instant) -> Prepared {
        let mut deadline = Deadline::new(overall);
        let mut values = BTreeMap::new();
        for field in rule.inputs.iter().flat_map(|input| &input.fields) {
            deadline.record(
                Instant::now(),
                field.maxage.map(Duration::from_secs_f64),
                Arc::from(field.name.0.as_str()),
            );
            values.insert(
                field.name.0.clone(),
                Arc::new(crate::interpreter::adapter::InputValue::Number(1.0)),
            );
        }
        Prepared { values, deadline }
    }

    #[tokio::test]
    async fn queued_expiry_retries_before_evaluating_predicates() {
        let (adapter, server) = adapter().await;
        tokio::time::pause();
        let program = crate::srdl::parse(
            r#"trigger a before always;
            rule a {
            input from "sensor/a" select number x maxage 1;
            require x / 0 > 0; enforce deny; }"#,
        )
        .unwrap();
        let mut pool = Pool::new(&program, &adapter);
        let key = ("plan".into(), "call".into(), TriggerTiming::Before);
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.check(Check {
            key: key.clone(),
            check_id: 1,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        });
        pool.acquiring.clear();
        let overall = pool.calls[&key].deadline;
        let stale = snapshot(&program.rules[0], overall);
        tokio::time::advance(Duration::from_secs(2)).await;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            data: Ok(stale),
        });
        assert_eq!(pool.rules[&(key.clone(), 0)].retries, 1);
        assert!(pool.calls[&key].results.is_empty());
        pool.acquiring.clear();
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            data: Ok(snapshot(&program.rules[0], overall)),
        });
        let result = receive.await.unwrap();
        assert_eq!(result.effect, Effect::Deny);
        assert!(matches!(
            result.reports[0].evaluation,
            Err(Error::Expression {
                source: super::super::expression::Error::DivisionByZero,
                ..
            })
        ));
        assert!(pool.calls.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn retries_expired_results_at_the_combined_decision_and_bounds_attempts() {
        let (adapter, server) = adapter().await;
        tokio::time::pause();
        let program = crate::srdl::parse(
            r#"
            trigger [a, b] before always;
            trigger [a, b] after always;
            rule a { input from "sensor/a" select number x maxage 1;
                require x > 0; enforce deny; }
            rule b { input from "sensor/b" select number x;
                require x > 0; enforce deny; }
        "#,
        )
        .unwrap();
        let mut pool = Pool::new(&program, &adapter);
        let key = ("plan".into(), "call".into(), TriggerTiming::Before);
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.check(Check {
            key: key.clone(),
            check_id: 1,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        });
        // Supply timestamped snapshots directly so network latency cannot affect expiry.
        pool.acquiring.clear();
        let deadline = pool.calls[&key].deadline;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            data: Ok(snapshot(&program.rules[0], deadline)),
        });
        tokio::time::advance(Duration::from_secs(2)).await;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 1),
            check_id: 1,
            data: Ok(snapshot(&program.rules[1], deadline)),
        });
        assert_eq!(pool.rules[&(key.clone(), 0)].retries, 1);
        assert_eq!(pool.calls[&key].results.len(), 1);
        assert_eq!(pool.calls[&key].deadline, deadline);
        pool.acquiring.clear();
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            data: Ok(snapshot(&program.rules[0], deadline)),
        });
        assert_eq!(receive.await.unwrap().effect, Effect::Allow);
        assert!(pool.rules.is_empty());
        assert!(pool.calls.is_empty());

        let key = ("plan".into(), "call".into(), TriggerTiming::After);
        let (_after_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.check(Check {
            key: key.clone(),
            check_id: 2,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        });
        assert!(pool.calls[&key].deadline > deadline);
        assert_eq!(pool.rules[&(key.clone(), 0)].retries, 0);
        pool.acquiring.clear();
        let deadline = pool.calls[&key].deadline;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 1),
            check_id: 2,
            data: Ok(snapshot(&program.rules[1], deadline)),
        });
        for attempt in 0..=MAX_FRESHNESS_RETRIES {
            pool.evaluate(ReadyCheck {
                key: (key.clone(), 0),
                check_id: 2,
                data: Err(Error::Expired {
                    rule: "a".into(),
                    input: "x".into(),
                }),
            });
            pool.acquiring.clear();
            if attempt < MAX_FRESHNESS_RETRIES {
                assert_eq!(pool.rules[&(key.clone(), 0)].retries, attempt + 1);
                assert_eq!(pool.calls[&key].deadline, deadline);
            }
        }
        let result = receive.await.unwrap();
        assert_eq!(result.effect, Effect::Allow);
        assert_eq!(result.reports[0].timing, TriggerTiming::After);
        assert!(matches!(
            result.reports[0].evaluation,
            Err(Error::Expired { .. })
        ));
        assert!(pool.calls.is_empty());
        assert!(pool.rules.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn cancelled_reuse_ignores_old_work_and_releases_capacity_after_checks() {
        let (adapter, server) = adapter().await;
        let program = crate::srdl::parse(
            r#"trigger a before always;
            rule a { input from "sensor/a" select number x; require x > 0; enforce deny; }"#,
        )
        .unwrap();
        let mut pool = Pool::new(&program, &adapter);
        let permits = Arc::new(Semaphore::new(2));
        let key = ("plan".to_owned(), "call".to_owned(), TriggerTiming::Before);
        let (owner, cancel) = watch::channel(());
        let (reply, old_receive) = oneshot::channel();
        pool.check(Check {
            key: key.clone(),
            check_id: 1,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: permits.clone().try_acquire_owned().unwrap(),
        });
        pool.acquiring.clear();
        drop(owner);
        // Reuse happens before the cancellation stream has been polled.
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.check(Check {
            key: key.clone(),
            check_id: 2,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: permits.clone().try_acquire_owned().unwrap(),
        });
        pool.acquiring.clear();
        assert!(old_receive.await.is_err());
        let deadline = pool.calls[&key].deadline;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            data: Ok(snapshot(&program.rules[0], deadline)),
        });
        assert!(pool.calls[&key].results.is_empty());
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 2,
            data: Ok(snapshot(&program.rules[0], deadline)),
        });
        assert!(pool.calls.is_empty());
        assert!(pool.rules.is_empty());
        // Check completion releases capacity even while its submitting owner is alive.
        timeout(Duration::from_secs(1), async {
            while let Some(event) = pool.cancelled.next().await {
                assert!(event.is_none());
            }
        })
        .await
        .unwrap();
        assert_eq!(permits.available_permits(), 2);
        assert_eq!(receive.await.unwrap().effect, Effect::Allow);
        server.abort();
    }
}
