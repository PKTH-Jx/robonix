// SPDX-License-Identifier: MulanPSL-2.0

//! Binding names needed by one rule's preconditions or postconditions.

use std::{collections::BTreeSet, convert::Infallible};

use crate::srdl::{
    ast::{Identifier, Rule},
    visitor::Visitor,
};

/// Collects binding references without changing the immutable expression tree.
#[derive(Default)]
pub(super) struct References<'ast> {
    /// Unique aliases used by the selected conditions.
    names: BTreeSet<&'ast str>,
}

impl<'ast> Visitor<'ast> for References<'ast> {
    type Error = Infallible;
    fn visit_identifier(&mut self, name: &'ast Identifier) -> Result<(), Self::Error> {
        self.names.insert(&name.0);
        Ok(())
    }
}

impl<'ast> References<'ast> {
    /// Select only bindings referenced by this rule's conditions.
    pub(super) fn collect(rule: &'ast Rule) -> BTreeSet<&'ast str> {
        let mut references = References::default();
        for condition in &rule.requires {
            references.visit_expression(&condition.expression).unwrap();
        }
        references.names
    }
}
