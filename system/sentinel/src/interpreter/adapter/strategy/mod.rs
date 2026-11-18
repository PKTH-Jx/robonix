//! Extensible JSON processing for yielded rule inputs.

mod summarize;

pub use summarize::Summarize;

use std::{collections::BTreeMap, sync::Arc};

use futures::future::BoxFuture;
use serde_json::Value;

use super::InputValue;
use crate::srdl::ast::{ContextClause, Rule, YieldClause, YieldField};

/// A shared JSON transformation receiving configuration for each invocation.
pub trait Strategy: Send + Sync {
    fn process<'a>(
        &'a self,
        input: Value,
        arguments: &'a [String],
        fields: &'a [YieldField],
    ) -> BoxFuture<'a, anyhow::Result<Value>>;
}

/// Strategy implementations available to pipelines; empty by default.
#[derive(Clone, Default)]
pub struct Strategies {
    /// Implementations indexed by the name used in SRDL.
    strategies: BTreeMap<String, Arc<dyn Strategy>>,
}

impl Strategies {
    /// Register a strategy implementation for use in SRDL.
    pub fn register(&mut self, name: impl Into<String>, strategy: Arc<dyn Strategy>) {
        self.strategies.insert(name.into(), strategy);
    }

    /// Run a sequence of strategies on the input value, returning the final result.
    pub(super) async fn run(
        &self,
        clause: &YieldClause,
        mut input: Value,
        rule: &Rule,
        context: &Value,
    ) -> anyhow::Result<Value> {
        let arguments = clause
            .strategies
            .iter()
            .map(|step| {
                step.arguments
                    .iter()
                    .map(|arg| Parts::parse(arg))
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let names = arguments
            .iter()
            .flatten()
            .flat_map(Parts::bindings)
            .collect();
        let bindings = super::context::bind(rule, context, Some(&names))?;
        for (step, parts) in clause.strategies.iter().zip(arguments) {
            let arguments = parts
                .iter()
                .map(|parts| parts.render(&bindings))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let strategy = self
                .strategies
                .get(&step.name.0)
                .ok_or_else(|| anyhow::anyhow!("unregistered strategy '{}'", step.name.0))?;
            input = strategy
                .process(input, &arguments, &clause.fields)
                .await
                .map_err(|err| anyhow::anyhow!("strategy '{}': {err}", step.name.0))?;
        }
        Ok(input)
    }
}

/// One literal fragment or imported binding in a strategy argument.
enum Part<'a> {
    Text(&'a str),
    Binding(&'a str),
}

/// Parsed strategy argument supporting context validation and interpolation.
pub(crate) struct Parts<'a> {
    /// Literal fragments and context references borrowed from one argument.
    parts: Vec<Part<'a>>,
}

impl<'a> Parts<'a> {
    /// Parse `{name}` placeholders and `{{` / `}}` literal braces.
    pub(crate) fn parse(mut template: &'a str) -> anyhow::Result<Self> {
        let mut parts = Vec::new();
        while !template.is_empty() {
            let index = template.find(['{', '}']).unwrap_or(template.len());
            if index > 0 {
                parts.push(Part::Text(&template[..index]));
                template = &template[index..];
                continue;
            }
            if template.starts_with("{{") || template.starts_with("}}") {
                parts.push(Part::Text(&template[..1]));
                template = &template[2..];
                continue;
            }
            anyhow::ensure!(
                template.starts_with('{'),
                "unmatched '}}' in strategy argument"
            );
            let end = template
                .find('}')
                .ok_or_else(|| anyhow::anyhow!("unclosed context placeholder"))?;
            let name = &template[1..end];
            anyhow::ensure!(
                !name.is_empty()
                    && name.bytes().enumerate().all(|(i, b)| b == b'_'
                        || b.is_ascii_alphabetic()
                        || (i > 0 && b.is_ascii_digit())),
                "invalid context placeholder '{name}'"
            );
            parts.push(Part::Binding(name));
            template = &template[end + 1..];
        }
        Ok(Self { parts })
    }

    /// Validate that all context references are imported by the rule.
    pub(crate) fn validate(&self, contexts: &[ContextClause]) -> anyhow::Result<()> {
        for name in self.bindings() {
            anyhow::ensure!(
                contexts.iter().any(|binding| binding.name.0 == name),
                "unimported context '{name}'"
            );
        }
        Ok(())
    }

    /// Return an iterator over the names of the context bindings in the template.
    fn bindings(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.parts.iter().filter_map(|part| match part {
            Part::Binding(name) => Some(*name),
            Part::Text(_) => None,
        })
    }

    /// Substitute values once so braces within context data remain literal.
    fn render(&self, context: &BTreeMap<String, Arc<InputValue>>) -> anyhow::Result<String> {
        let mut output = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => output.push_str(text),
                Part::Binding(name) => match context.get(*name) {
                    Some(value) => match value.as_ref() {
                        InputValue::String(text) => output.push_str(text),
                        InputValue::Number(number) => output.push_str(&number.to_string()),
                        InputValue::Boolean(boolean) => output.push_str(&boolean.to_string()),
                    },
                    None => {
                        anyhow::bail!("strategy argument references unimported context '{name}'")
                    }
                },
            }
        }
        Ok(output)
    }
}
