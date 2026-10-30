//! Unique input and context dependencies and requirement readiness accounting.

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
};

use crate::executor::visitor::Visitor;
use crate::srdl::ast::{Identifier, Rule};

/// Readiness and cached result for one synchronous requirement calculation.
pub(super) struct RequirementState {
    /// Number of distinct referenced fields that have not arrived.
    pub remaining: usize,
    /// Calculated truth value; false remains a result rather than an error.
    pub result: Option<bool>,
}

/// Reverse dependencies used to update only requirements affected by an arriving field.
pub(super) struct DependencyPlan<'ast> {
    /// Requirement state in declaration order.
    pub requirements: Vec<RequirementState>,
    /// Field aliases mapped to the requirements that reference them.
    dependents: BTreeMap<&'ast str, Vec<usize>>,
}

/// Collects each referenced alias once, including repeated occurrences in an expression.
#[derive(Default)]
struct References<'ast> {
    /// Distinct aliases found while walking one requirement expression.
    names: BTreeSet<&'ast str>,
}

impl<'ast> Visitor<'ast> for References<'ast> {
    type Error = Infallible;

    fn visit_identifier(&mut self, name: &'ast Identifier) -> Result<(), Self::Error> {
        self.names.insert(&name.0);
        Ok(())
    }
}

impl<'ast> DependencyPlan<'ast> {
    pub fn new(rule: &'ast Rule) -> Self {
        let mut requirements = Vec::new();
        let mut dependents: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (index, require) in rule.requires.iter().enumerate() {
            let mut references = References::default();
            references.visit_expression(&require.expression).unwrap();
            requirements.push(RequirementState {
                remaining: references.names.len(),
                result: None,
            });
            for name in references.names {
                dependents.entry(name).or_default().push(index);
            }
        }
        Self {
            requirements,
            dependents,
        }
    }

    /// The coordinator calls this only on the first arrival of a field alias.
    pub fn arrived(&mut self, name: &str) {
        if let Some(indices) = self.dependents.get(name) {
            for &index in indices {
                self.requirements[index].remaining -= 1;
            }
        }
    }
}
