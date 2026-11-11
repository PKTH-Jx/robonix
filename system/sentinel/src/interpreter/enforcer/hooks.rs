use super::{EnforcementBuilder, Error};
use crate::srdl::ast::{EnforceBehavior, EnforceClause, TriggerTiming};

/// Executor operations available to inherent hooks without an Executor dependency.
#[tonic::async_trait]
pub trait HookRuntime: Send + Sync {
    /// Complete one recorded capability call through the embedding runtime's lifecycle.
    async fn call(&self, capability: &str) -> Result<(), Error>;

    /// Attempt every recorded call in order, even after denial or an earlier call failure.
    async fn execute(&self, builder: &mut EnforcementBuilder, timing: TriggerTiming) {
        for index in 0..builder.calls().len() {
            let result = self.call(&builder.calls()[index].capability).await;
            builder.complete_call(index, result, timing);
        }
    }

    /// Resolve when the invocation is cancelled; the default never requests cancellation.
    async fn cancelled(&self) {
        std::future::pending::<()>().await;
    }
}

#[tonic::async_trait]
impl HookRuntime for () {
    async fn call(&self, _: &str) -> Result<(), Error> {
        Err(Error::Runtime(
            "capability hook requires an execution runtime".into(),
        ))
    }
}

/// A reusable built-in response with load-time validation and invocation-local updates.
pub trait Hook: Send + Sync {
    /// Name used to identify this hook in load-time diagnostics.
    fn name(&self) -> &'static str;

    /// Timings the type checker accepts for every trigger bound to this hook's rule.
    fn timings(&self) -> &'static [TriggerTiming] {
        &[TriggerTiming::Before, TriggerTiming::After]
    }

    /// Validate the hook's configuration at load time, before any rule evaluation.
    fn validate(&self) -> Result<(), Error> {
        Ok(())
    }

    /// Update the invocation's builder synchronously; capability calls must be queued.
    fn execute(&self, builder: &mut EnforcementBuilder);
}

/// Preserves permission contributed by other rules without overriding denial.
struct Allow;

impl Hook for Allow {
    fn name(&self) -> &'static str {
        "allow"
    }

    fn execute(&self, _: &mut EnforcementBuilder) {}
}

/// Rejects admission before the original capability can be dispatched.
struct Deny;

impl Hook for Deny {
    fn name(&self) -> &'static str {
        "deny"
    }

    fn timings(&self) -> &'static [TriggerTiming] {
        &[TriggerTiming::Before]
    }

    fn execute(&self, builder: &mut EnforcementBuilder) {
        builder.deny();
    }
}

/// Records a configured capability for execution by the embedding runtime.
struct Call {
    /// Contract ID resolved by Executor when this hook executes.
    capability: String,
}

impl Hook for Call {
    fn name(&self) -> &'static str {
        "call"
    }

    fn validate(&self) -> Result<(), Error> {
        if self.capability.trim().is_empty() {
            return Err(Error::Runtime(
                "call requires a nonempty capability ID".into(),
            ));
        }
        Ok(())
    }

    fn execute(&self, builder: &mut EnforcementBuilder) {
        builder.call(&self.capability);
    }
}

/// Appends the clause's static report text to the shared enforcement feedback.
struct Report {
    /// Decoded SRDL message retained until this hook executes.
    message: String,
}

impl Hook for Report {
    fn name(&self) -> &'static str {
        "report"
    }

    fn execute(&self, builder: &mut EnforcementBuilder) {
        builder.report(&self.message);
    }
}

fn builtin(behavior: &EnforceBehavior) -> Box<dyn Hook> {
    match behavior {
        EnforceBehavior::Allow => Box::new(Allow),
        EnforceBehavior::Deny => Box::new(Deny),
        EnforceBehavior::Call(capability) => Box::new(Call {
            capability: capability.clone(),
        }),
        EnforceBehavior::Report(message) => Box::new(Report {
            message: message.clone(),
        }),
    }
}

/// Use the same built-in bindings for static validation and runtime enforcement.
pub(crate) fn bind(clause: &EnforceClause) -> Vec<Box<dyn Hook>> {
    clause.hooks.iter().map(builtin).collect()
}
