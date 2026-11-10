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
    /// Select only bindings needed by the active clause group, deferring postcondition inputs.
    pub(super) fn collect(rule: &'ast Rule, ensure: bool) -> BTreeSet<&'ast str> {
        let mut references = References::default();
        if ensure {
            for condition in &rule.ensures {
                references.visit_expression(&condition.expression).unwrap();
            }
        } else {
            for condition in &rule.requires {
                references.visit_expression(&condition.expression).unwrap();
            }
        }
        references.names
    }
}
