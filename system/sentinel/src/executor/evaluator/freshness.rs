//! Monotonic input deadlines, including the fixed first-result timeout.

use std::{collections::BTreeMap, time::Duration};

use tokio::time::Instant;

use super::Error;
use crate::srdl::ast::Rule;

/// Timing constraints and receipt state for one declared field.
struct FieldTime {
    /// Maximum age rounded to clock resolution, plus the first invalid nanosecond.
    lifetime: Duration,
    /// First invalid instant; None means the field has not arrived.
    expires_at: Option<Instant>,
}

/// Freshness state for every declared field, including already evaluated or unused inputs.
pub(super) struct Freshness<'ast> {
    /// The rule used to attach context to timing errors.
    rule: &'ast Rule,
    /// Per-field timing requirements and deadlines.
    fields: BTreeMap<&'ast str, FieldTime>,
    /// Initial waiting deadline, used only until the first field arrives.
    first_deadline: Instant,
}

impl<'ast> Freshness<'ast> {
    pub fn new(rule: &'ast Rule, now: Instant, first_timeout: Duration) -> Result<Self, Error> {
        let mut fields = BTreeMap::new();
        for field in rule.inputs.iter().flat_map(|input| &input.fields) {
            let invalid = || Error::InvalidMaxage {
                rule: rule.name.0.clone(),
                input: field.name.0.clone(),
            };
            if field.maxage <= 0.0 {
                return Err(invalid());
            }
            let lifetime = Duration::try_from_secs_f64(field.maxage)
                .map_err(|_| invalid())?
                .checked_add(Duration::from_nanos(1))
                .ok_or_else(invalid)?;
            fields.insert(
                field.name.0.as_str(),
                FieldTime {
                    lifetime,
                    expires_at: None,
                },
            );
        }
        Ok(Self {
            rule,
            fields,
            first_deadline: now + first_timeout,
        })
    }

    pub fn record(&mut self, name: &str, acquired_at: Instant, now: Instant) -> Result<(), Error> {
        if acquired_at > now {
            return Err(Error::FutureTimestamp {
                rule: self.rule.name.0.clone(),
                input: name.into(),
            });
        }
        let field = self
            .fields
            .get_mut(name)
            .expect("Adapter fields belong to the evaluated rule");
        field.expires_at =
            Some(
                acquired_at
                    .checked_add(field.lifetime)
                    .ok_or_else(|| Error::InvalidMaxage {
                        rule: self.rule.name.0.clone(),
                        input: name.into(),
                    })?,
            );
        Ok(())
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.fields
            .values()
            .filter_map(|field| field.expires_at)
            .min()
            .or_else(|| (!self.fields.is_empty()).then_some(self.first_deadline))
    }

    pub fn check(&self, now: Instant) -> Result<(), Error> {
        for (name, field) in &self.fields {
            if field.expires_at.is_some_and(|deadline| now >= deadline) {
                return Err(Error::Expired {
                    rule: self.rule.name.0.clone(),
                    input: (*name).into(),
                });
            }
        }
        if !self.fields.is_empty()
            && self.fields.values().all(|field| field.expires_at.is_none())
            && now >= self.first_deadline
        {
            return Err(Error::FirstInputTimeout {
                rule: self.rule.name.0.clone(),
            });
        }
        Ok(())
    }

    pub fn complete(&self) -> bool {
        self.fields.values().all(|field| field.expires_at.is_some())
    }
}
