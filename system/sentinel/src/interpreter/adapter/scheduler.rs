// SPDX-License-Identifier: MulanPSL-2.0

//! Round ordering shared by all active input acquisitions.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{oneshot, watch},
    time::Instant,
};

use super::{InputErrorKind, transport};
use crate::{interpreter::evaluator::Error, srdl::ast::Rule};

/// Adapter-computed validity limit retained with the rule's observation snapshot.
#[derive(Clone)]
pub(in crate::interpreter) struct Deadline {
    /// First invalid instant, bounded by the original activation budget.
    pub at: Instant,
    /// Original timeout, which takes precedence over input expiry and never resets on retry.
    pub overall: Instant,
    /// Field responsible for a freshness deadline, absent when only the overall timeout applies.
    pub input: Option<Arc<str>>,
}

impl Deadline {
    /// Check overall timeout first so an exhausted budget cannot trigger a freshness retry.
    pub(in crate::interpreter) fn check(&self, rule: &Rule, now: Instant) -> Result<(), Error> {
        if now >= self.overall {
            return Err(Error::Timeout {
                rule: rule.name.0.clone(),
            });
        }
        if now >= self.at {
            return Err(Error::Expired {
                rule: rule.name.0.clone(),
                input: self
                    .input
                    .as_deref()
                    .expect("a freshness deadline identifies its input")
                    .into(),
            });
        }
        Ok(())
    }
    pub fn new(overall: Instant) -> Self {
        Self {
            at: overall,
            overall,
            input: None,
        }
    }

    /// Exact maxage is valid; queue and evaluator share the first invalid nanosecond.
    pub fn record(
        &mut self,
        acquired_at: Instant,
        maxage: Option<Duration>,
        input: Arc<str>,
    ) -> bool {
        let Some(expiry) = maxage
            .and_then(|age| age.checked_add(Duration::from_nanos(1)))
            .and_then(|age| acquired_at.checked_add(age))
        else {
            return false;
        };
        if expiry >= self.at {
            return false;
        }
        self.at = expiry;
        self.input = Some(input);
        true
    }
}

/// One capability invocation waiting for an acquisition slot.
pub(super) struct Request {
    /// Contract to discover and invoke without arguments.
    pub capability: String,
    /// Atlas consumer identity retained independently of the rule stream.
    pub consumer: String,
    /// Closing the receiver cancels this request, including in-flight transport work.
    pub reply: oneshot::Sender<Result<transport::Response, InputErrorKind>>,
    /// Smallest maxage among selected fields sharing this response; None is unlimited.
    pub maxage: Option<Duration>,
    /// Field alias reported when this response sets the freshness deadline.
    pub input: Arc<str>,
}

/// Remaining inputs and the priority of one active rule attempt.
struct RuleQueue {
    /// Inputs ordered from longest to shortest maxage, with declaration-order ties.
    requests: VecDeque<Request>,
    /// Round in which this rule may dispatch its next input.
    round: usize,
    /// Minimum of the overall deadline and all received field expiry times.
    deadline: watch::Sender<Deadline>,
}

/// Queue state protected only during insertion, selection, or completion bookkeeping.
#[derive(Default)]
pub(super) struct Scheduler {
    /// Rule attempts indexed by monotonically increasing registration sequence.
    rules: BTreeMap<u64, RuleQueue>,
    /// Round first, then deadline, then FIFO registration order.
    ready: BTreeSet<(usize, Instant, u64)>,
    /// Workers keep their slot until transport cleanup has finished.
    pub active: usize,
    /// Distinguishes retries and late completions from retired rule attempts.
    next_id: u64,
    /// Last dispatched round, used when arrivals find no queued peers.
    round: usize,
}

impl Scheduler {
    pub fn insert(
        &mut self,
        mut requests: Vec<Request>,
        overall: Instant,
    ) -> (u64, watch::Receiver<Deadline>) {
        let (deadline, receive) = watch::channel(Deadline::new(overall));
        let id = self.next_id;
        self.next_id += 1;
        // New checks and retries join their waiting peers without catching up from zero.
        let round = self.ready.first().map_or(self.round, |key| key.0);
        // Unlimited fields can be fetched early; short-lived observations should arrive last.
        requests.sort_by(|a, b| match (a.maxage, b.maxage) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => b.cmp(&a),
        });
        if !requests.is_empty() {
            self.ready.insert((round, overall, id));
        }
        self.rules.insert(
            id,
            RuleQueue {
                requests: requests.into(),
                round,
                deadline,
            },
        );
        (id, receive)
    }

    /// Count starts rather than completions so spare slots can fetch the next round.
    pub fn next(&mut self, now: Instant) -> Option<(u64, Request)> {
        while let Some((round, deadline, id)) = self.ready.pop_first() {
            let rule = self.rules.get_mut(&id).unwrap();
            if now >= rule.deadline.borrow().overall {
                rule.requests.clear();
                continue;
            }
            let request = rule.requests.pop_front().unwrap();
            self.round = round;
            rule.round += 1;
            if !rule.requests.is_empty() {
                self.ready.insert((round + 1, deadline, id));
            }
            self.active += 1;
            return Some((id, request));
        }
        None
    }

    /// Reinsert the existing queue key only when a response shortens its deadline.
    pub fn received(
        &mut self,
        id: u64,
        acquired_at: Instant,
        maxage: Option<Duration>,
        input: Arc<str>,
    ) {
        let Some(rule) = self.rules.get_mut(&id) else {
            return;
        };
        let mut deadline = rule.deadline.borrow().clone();
        let previous = deadline.at;
        if deadline.record(acquired_at, maxage, input) {
            self.ready.remove(&(rule.round, previous, id));
            if !rule.requests.is_empty() {
                self.ready.insert((rule.round, deadline.at, id));
            }
            rule.deadline.send_replace(deadline);
        }
    }

    pub fn complete(&mut self) {
        self.active -= 1;
    }

    pub fn cancel(&mut self, id: u64) {
        if let Some(rule) = self.rules.remove(&id) {
            self.ready
                .remove(&(rule.round, rule.deadline.borrow().at, id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, maxage: Option<u64>) -> Request {
        let (reply, _receive) = oneshot::channel();
        Request {
            capability: name.into(),
            consumer: "test".into(),
            reply,
            maxage: maxage.map(Duration::from_millis),
            input: Arc::from(name),
        }
    }

    #[test]
    fn interleaves_rules_before_starting_their_next_inputs() {
        let now = Instant::now();
        let mut queue = Scheduler::default();
        let (a, _) = queue.insert(
            vec![request("A2", Some(100)), request("A1", Some(1000))],
            now + Duration::from_millis(1000),
        );
        let (b, _) = queue.insert(
            vec![request("B2", Some(100)), request("B1", Some(500))],
            now + Duration::from_millis(700),
        );
        let (id, first) = queue.next(now).unwrap();
        assert_eq!((id, first.capability.as_str()), (b, "B1"));
        queue.received(id, now, Some(Duration::from_millis(600)), Arc::from("B1"));
        queue.complete();
        let (id, second) = queue.next(now).unwrap();
        assert_eq!((id, second.capability.as_str()), (a, "A1"));
        queue.received(id, now, Some(Duration::from_millis(1300)), Arc::from("A1"));
        queue.complete();
        assert_eq!(queue.next(now).unwrap().1.capability, "B2");
        // A2 can start while B2 is in flight; rounds do not impose completion barriers.
        assert_eq!(queue.next(now).unwrap().1.capability, "A2");
        assert!(queue.next(now).is_none());
    }

    #[test]
    fn updates_deadline_keys_and_removes_cancelled_or_timed_out_work() {
        let now = Instant::now();
        let overall = now + Duration::from_secs(5);
        let mut queue = Scheduler::default();
        let (a, _) = queue.insert(vec![request("A1", None), request("A2", None)], overall);
        let (b, _) = queue.insert(vec![request("B1", None), request("B2", None)], overall);
        assert_eq!(queue.next(now).unwrap().0, a);
        assert_eq!(queue.next(now).unwrap().0, b);
        // A response reorders the next round while its cleanup still occupies a slot.
        queue.received(b, now, Some(Duration::from_secs(1)), Arc::from("B1"));
        assert_eq!(queue.active, 2);
        queue.complete();
        queue.complete();
        assert_eq!(queue.next(now).unwrap().1.capability, "B2");
        queue.cancel(b);
        // Late cleanup still releases its slot without recreating the cancelled attempt.
        queue.received(b, now, Some(Duration::ZERO), Arc::from("B1"));
        queue.complete();
        assert!(queue.next(overall).is_none());
        assert_eq!(queue.active, 0);
        assert!(queue.ready.is_empty());
    }

    #[test]
    fn fetches_unbounded_and_long_lived_inputs_first_with_stable_ties() {
        let now = Instant::now();
        let mut queue = Scheduler::default();
        queue.insert(
            vec![
                request("short", Some(1)),
                request("long1", Some(100)),
                request("unbounded", None),
                request("long2", Some(100)),
            ],
            now + Duration::from_secs(5),
        );
        let mut order = Vec::new();
        while let Some((_, request)) = queue.next(now) {
            order.push(request.capability);
        }
        assert_eq!(order, ["unbounded", "long1", "long2", "short"]);
    }

    #[test]
    fn new_rules_join_the_current_round_without_catch_up_priority() {
        let now = Instant::now();
        let mut queue = Scheduler::default();
        queue.insert(
            vec![
                request("A1", None),
                request("A2", None),
                request("A3", None),
            ],
            now + Duration::from_secs(1),
        );
        assert_eq!(queue.next(now).unwrap().1.capability, "A1");
        assert_eq!(queue.next(now).unwrap().1.capability, "A2");
        queue.insert(
            vec![request("B1", None), request("B2", None)],
            now + Duration::from_secs(2),
        );
        assert_eq!(queue.next(now).unwrap().1.capability, "A3");
        assert_eq!(queue.next(now).unwrap().1.capability, "B1");
    }

    #[test]
    fn publishes_receipt_deadlines_and_retains_them_after_acquisition() {
        let now = Instant::now();
        let overall = now + Duration::from_secs(5);
        let mut queue = Scheduler::default();
        let (id, mut deadlines) = queue.insert(vec![request("sensor", Some(1000))], overall);
        queue.next(now).unwrap();
        queue.received(id, now, Some(Duration::from_secs(1)), Arc::from("x"));
        assert!(deadlines.has_changed().unwrap());
        let snapshot = deadlines.borrow_and_update().clone();
        assert_eq!(
            snapshot.at,
            now + Duration::from_secs(1) + Duration::from_nanos(1)
        );
        assert_eq!(snapshot.overall, overall);
        assert_eq!(snapshot.input.as_deref(), Some("x"));
        assert_eq!(queue.active, 1);
        queue.complete();
        queue.cancel(id);
        assert_eq!(snapshot.at, deadlines.borrow().at);
        assert!(deadlines.has_changed().is_err());
    }
}
