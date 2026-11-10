// SPDX-License-Identifier: MulanPSL-2.0

//! One owner for invocation-bound rule states, acquisitions, and FIFO evaluation.

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
    sync::{OwnedSemaphorePermit, mpsc, oneshot, watch},
    time::Instant,
};

use super::{CallKey, Command, Error, RULE_TIMEOUT, RuleEvaluator, RuleResult};
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
    Pending,
    Waiting,
    Evaluating,
    Enforcing,
}

/// Processing state retained for a matching rule until its applicable checks finish.
struct RuleEntry {
    /// Preconditions are removed after their combined enforcement decision.
    require: Option<Status>,
    /// Postconditions remain pending until successful execution is reported.
    ensure: Option<Status>,
    /// Cancels acquisition while preserving the Adapter's connection cleanup.
    acquisition: Option<AbortHandle>,
    /// Retained deadlines keep early results valid through combined enforcement.
    freshness: Option<Deadline>,
    /// Bounds reacquisition of the active clause group when its snapshot expires.
    retries: usize,
}

/// Invocation metadata shared by every rule matched for one call.
struct Invocation {
    /// Distinguishes late responses from a later reuse of the same call key.
    check_id: u64,
    /// Atlas identity used for acquiring rule inputs.
    consumer: Arc<str>,
    /// Trusted arguments and identity, fixed across both condition groups.
    context: Arc<Value>,
    /// Source indices retained so completion never repeats trigger matching.
    rules: Vec<usize>,
    /// Completed results ordered by their rule's source index.
    results: BTreeMap<usize, Result<RuleResult, Error>>,
    /// Delivers the current precondition or postcondition decision.
    reply: Option<oneshot::Sender<EnforcementResult>>,
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
    /// Selects the entry's postconditions rather than its preconditions.
    ensure: bool,
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
    /// Combined decisions and lifetime metadata for each invocation.
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
    pub(super) async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            tokio::select! {
                // Process commands in arrival order
                command = commands.recv() => match command {
                    Some(command) => self.command(command),
                    None => break,
                },
                // Process acquisition completions
                Some(result) = self.acquiring.next(), if !self.acquiring.is_empty() => {
                    if let Some(result) = result
                        && self.current(&result.key.0, result.check_id)
                        && let Some(entry) = self.rules.get_mut(&result.key) {
                                let status = if result.ensure { &mut entry.ensure } else { &mut entry.require };
                                *status = Some(Status::Evaluating);
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

    /// Dispatch one command without holding a lock across any await points.
    fn command(&mut self, command: Command) {
        match command {
            Command::Begin {
                key,
                check_id,
                capability,
                consumer,
                context,
                cancel,
                reply,
                permit,
            } => {
                self.begin(
                    key, check_id, capability, consumer, context, cancel, reply, permit,
                );
            }
            Command::Complete {
                key,
                check_id,
                reply,
            } => self.complete(key, check_id, reply),
        }
    }

    /// Match once and retain admission capacity until cancellation work is retired.
    #[allow(
        clippy::too_many_arguments,
        reason = "The handler takes ownership of every Command::Begin field."
    )]
    fn begin(
        &mut self,
        key: CallKey,
        check_id: u64,
        capability: String,
        consumer: Arc<str>,
        context: Arc<Value>,
        cancel: watch::Receiver<()>,
        reply: oneshot::Sender<EnforcementResult>,
        permit: OwnedSemaphorePermit,
    ) {
        if self
            .calls
            .get(&key)
            .is_some_and(|call| call.owner.has_changed().is_err())
        {
            self.remove(&key);
        }
        if self.calls.contains_key(&key) {
            let _ = reply.send(EnforcementResult::failure(
                "Capability call already has active Sentinel checks",
                false,
            ));
            return;
        }
        let rules = TriggerMatcher::match_indices(self.program, &capability);
        super::super::logging::log(
            robonix_scribe::Level::Info,
            format!(
                "[sentinel] check_id={check_id} capability={capability:?} consumer={consumer:?} triggered_rules={:?}",
                rules
                    .iter()
                    .map(|index| self.program.rules[*index].name.0.as_str())
                    .collect::<Vec<_>>()
            ),
        );
        for index in &rules {
            self.rules.insert(
                (key.clone(), *index),
                RuleEntry {
                    require: Some(Status::Waiting),
                    ensure: (!self.program.rules[*index].ensures.is_empty())
                        .then_some(Status::Pending),
                    acquisition: None,
                    freshness: None,
                    retries: 0,
                },
            );
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
                reply: Some(reply),
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
        self.acquire(&key, rules, false);
        if self.calls[&key].rules.is_empty() {
            self.enforce(&key, false);
        }
    }

    /// Activate retained postconditions with a new budget after successful capability execution.
    fn complete(&mut self, key: CallKey, check_id: u64, reply: oneshot::Sender<EnforcementResult>) {
        if !self.current(&key, check_id) {
            let _ = reply.send(EnforcementResult::failure(
                "No retained Sentinel checks for this invocation",
                true,
            ));
            return;
        }
        let indices = self.calls[&key].rules.clone();
        let call = self.calls.get_mut(&key).unwrap();
        call.reply = Some(reply);
        call.deadline = Instant::now() + RULE_TIMEOUT;
        for index in &indices {
            self.rules.get_mut(&(key.clone(), *index)).unwrap().retries = 0;
        }
        self.acquire(&key, indices.clone(), true);
        if indices.is_empty() {
            self.enforce(&key, true);
        }
    }

    /// Register peers together before fetching, then retain one atomic snapshot per rule.
    fn acquire(&mut self, key: &CallKey, indices: Vec<usize>, ensure: bool) {
        let call = &self.calls[key];
        let check_id = call.check_id;
        let deadline = call.deadline;
        let datasets = self.adapter.acquire_group(
            indices.iter().map(|index| &self.program.rules[*index]),
            &call.consumer,
            call.context.clone(),
            check_id,
            ensure,
            deadline,
        );
        for (index, dataset) in indices.into_iter().zip(datasets) {
            let key = (key.clone(), index);
            let (abort, registration) = AbortHandle::new_pair();
            let entry = self.rules.get_mut(&key).unwrap();
            let status = if ensure {
                &mut entry.ensure
            } else {
                &mut entry.require
            };
            *status = Some(Status::Waiting);
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
                                ensure,
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
                        let result =
                            RuleEvaluator::evaluate(ready.check_id, rule, ready.ensure, &prepared)?;
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
            self.acquire(&ready.key.0, vec![ready.key.1], ready.ensure);
            return;
        }
        let entry = self.rules.get_mut(&ready.key).unwrap();
        if ready.ensure {
            entry.ensure = Some(Status::Enforcing);
        } else {
            entry.require = Some(Status::Enforcing);
        }
        let call = self.calls.get_mut(&ready.key.0).unwrap();
        call.results.insert(ready.key.1, result);
        if call.results.len() == call.rules.len() {
            self.enforce(&ready.key.0, ready.ensure);
        }
    }

    /// Combine every matched rule, rechecking snapshots that finished before slower rules.
    fn enforce(&mut self, key: &CallKey, ensure: bool) {
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
            self.acquire(key, retries, ensure);
            return;
        }
        let mut decision = Enforcer::enforce(
            completed
                .into_iter()
                .map(|(index, result)| (&self.program.rules[index], result)),
        );
        for report in &mut decision.reports {
            report.ensure = ensure;
        }
        super::super::SrdlInterpreter::log_decision(call.check_id, &decision);
        let denied = decision.effect == Effect::Deny;
        if let Some(reply) = call.reply.take() {
            let _ = reply.send(decision);
        }
        // Keep only rules with postconditions after precondition enforcement.
        if ensure || denied {
            self.remove(key);
        } else {
            call.rules.retain(|index| {
                let rule_key = (key.clone(), *index);
                if let Some(entry) = self.rules.get_mut(&rule_key) {
                    entry.require = None;
                    if entry.ensure.is_none() {
                        self.rules.remove(&rule_key);
                        return false;
                    }
                }
                true
            });
        }
    }

    /// Abort acquisition and retire the watcher even if its caller never polls the reply.
    fn remove(&mut self, key: &CallKey) {
        if let Some(call) = self.calls.remove(key) {
            call.cancellation.abort();
            for index in call.rules {
                if let Some(entry) = self.rules.remove(&(key.clone(), index))
                    && let Some(acquisition) = entry.acquisition
                {
                    acquisition.abort();
                }
            }
            self.ready
                .retain(|ready| &ready.key.0 != key || ready.check_id != call.check_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            r#"trigger a @ always;
            rule a {
            input from "sensor/a" select number x maxage 1;
            require x / 0 > 0; enforce deny; }"#,
        )
        .unwrap();
        let mut pool = Pool::new(&program, &adapter);
        let key = ("plan".into(), "call".into());
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.command(Command::Begin {
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
            ensure: false,
            data: Ok(stale),
        });
        assert_eq!(pool.rules[&(key.clone(), 0)].retries, 1);
        assert!(pool.calls[&key].results.is_empty());
        pool.acquiring.clear();
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            ensure: false,
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
            trigger [a, b] @ always;
            rule a { input from "sensor/a" select number x maxage 1;
                require x > 0; ensure x > 0; enforce deny; }
            rule b { input from "sensor/b" select number x;
                require x > 0; ensure x > 0; enforce deny; }
        "#,
        )
        .unwrap();
        let mut pool = Pool::new(&program, &adapter);
        let key = ("plan".into(), "call".into());
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.command(Command::Begin {
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
            ensure: false,
            data: Ok(snapshot(&program.rules[0], deadline)),
        });
        tokio::time::advance(Duration::from_secs(2)).await;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 1),
            check_id: 1,
            ensure: false,
            data: Ok(snapshot(&program.rules[1], deadline)),
        });
        assert_eq!(pool.rules[&(key.clone(), 0)].retries, 1);
        assert_eq!(pool.calls[&key].results.len(), 1);
        assert_eq!(pool.calls[&key].deadline, deadline);
        pool.acquiring.clear();
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 1,
            ensure: false,
            data: Ok(snapshot(&program.rules[0], deadline)),
        });
        assert_eq!(receive.await.unwrap().effect, Effect::Allow);

        let (reply, receive) = oneshot::channel();
        pool.command(Command::Complete {
            key: key.clone(),
            check_id: 1,
            reply,
        });
        assert!(pool.calls[&key].deadline > deadline);
        assert_eq!(pool.rules[&(key.clone(), 0)].retries, 0);
        pool.acquiring.clear();
        let deadline = pool.calls[&key].deadline;
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 1),
            check_id: 1,
            ensure: true,
            data: Ok(snapshot(&program.rules[1], deadline)),
        });
        for attempt in 0..=MAX_FRESHNESS_RETRIES {
            pool.evaluate(ReadyCheck {
                key: (key.clone(), 0),
                check_id: 1,
                ensure: true,
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
        assert_eq!(result.effect, Effect::Deny);
        assert!(matches!(
            result.reports[0].evaluation,
            Err(Error::Expired { .. })
        ));
        assert!(pool.calls.is_empty());
        assert!(pool.rules.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn reuse_and_completion_retire_watchers_even_if_owners_remain_alive() {
        let (adapter, server) = adapter().await;
        let program = crate::srdl::parse("").unwrap();
        let mut pool = Pool::new(&program, &adapter);
        let permits = Arc::new(Semaphore::new(2));
        let key = ("plan".to_owned(), "call".to_owned());
        let (owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.command(Command::Begin {
            key: key.clone(),
            check_id: 1,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: permits.clone().try_acquire_owned().unwrap(),
        });
        assert_eq!(receive.await.unwrap().effect, Effect::Allow);
        drop(owner);
        // Reuse happens before the cancellation stream has been polled.
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        pool.command(Command::Begin {
            key: key.clone(),
            check_id: 2,
            capability: "robot/pick".into(),
            consumer: Arc::from("test"),
            context: Arc::new(Value::Null),
            cancel,
            reply,
            permit: permits.clone().try_acquire_owned().unwrap(),
        });
        assert_eq!(receive.await.unwrap().effect, Effect::Allow);
        assert!(pool.current(&key, 2));
        let (reply, receive) = oneshot::channel();
        pool.command(Command::Complete {
            key,
            check_id: 2,
            reply,
        });
        assert!(pool.calls.is_empty());
        assert!(pool.rules.is_empty());
        // Keep both completion receiver and owner alive while retiring the watchers.
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
