//! Extensible JSON processing for yielded rule inputs.

mod summarize;

pub use summarize::{Summarize, SummarizeConfig};

use std::{collections::BTreeMap, sync::Arc};

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;

use super::InputValue;
use crate::srdl::ast::{ContextClause, Program, Rule, YieldClause, YieldField};

/// Explicitly enabled strategies and their initialization settings.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyConfig {
    /// Model configuration; omission leaves Summarize unavailable to rules.
    #[serde(rename = "Summarize")]
    pub summarize: Option<SummarizeConfig>,
}

impl std::fmt::Debug for StrategyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyConfig")
            .field(
                "Summarize",
                &self.summarize.as_ref().map(|_| "<configured>"),
            )
            .finish()
    }
}

/// A shared JSON transformation receiving configuration for each invocation.
pub trait Strategy: Send + Sync {
    /// Check positional arguments before the rule can acquire any observations.
    fn validate_arguments(&self, _arguments: &[String]) -> anyhow::Result<()> {
        Ok(())
    }

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
    pub(crate) fn initialize(config: StrategyConfig) -> anyhow::Result<Self> {
        let mut strategies = Self::default();
        if let Some(config) = config.summarize {
            strategies.register(
                "Summarize",
                Arc::new(Summarize::new(
                    &config.api_base,
                    &config.model,
                    &config.api_key,
                )?),
            );
        }
        Ok(strategies)
    }

    /// Validate that all strategies referenced in the program are configured and correct.
    pub(crate) fn validate(&self, program: &Program) -> anyhow::Result<()> {
        use anyhow::Context;
        for rule in &program.rules {
            for step in rule
                .inputs
                .iter()
                .flat_map(|input| &input.yields)
                .flat_map(|clause| &clause.strategies)
            {
                let strategy = self.strategies.get(&step.name.0).with_context(|| {
                    format!(
                        "rule '{}': strategy '{}' is not configured",
                        rule.name.0, step.name.0
                    )
                })?;
                strategy
                    .validate_arguments(&step.arguments)
                    .with_context(|| {
                        format!("rule '{}': strategy '{}'", rule.name.0, step.name.0)
                    })?;
            }
        }
        Ok(())
    }

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

    fn render_value(value: &InputValue) -> anyhow::Result<String> {
        match value {
            InputValue::String(text) => Ok(text.clone()),
            InputValue::Number(number) => Ok(number.to_string()),
            InputValue::Boolean(boolean) => Ok(boolean.to_string()),
            InputValue::Some(inner) => Self::render_value(inner),
            InputValue::None(_) => anyhow::bail!("cannot interpolate an absent context value"),
        }
    }

    /// Substitute values once so braces within context data remain literal.
    fn render(&self, context: &BTreeMap<String, Arc<InputValue>>) -> anyhow::Result<String> {
        let mut output = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => output.push_str(text),
                Part::Binding(name) => {
                    let value = context.get(*name).ok_or_else(|| {
                        anyhow::anyhow!("strategy argument references unimported context '{name}'")
                    })?;
                    output.push_str(&Self::render_value(value)?);
                }
            }
        }
        Ok(output)
    }
}
