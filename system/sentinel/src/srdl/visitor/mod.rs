//! Read-only AST traversal with fallible hooks and an analysis-specific expression result.

use super::ast::*;

pub mod type_check;

/// Override individual hooks and use walk functions to visit their children.
pub trait Visitor {
    type Error;
    type ExpressionOutput;

    fn visit_program(&mut self, program: &Program) -> Result<(), Self::Error> {
        walk_program(self, program)
    }

    fn visit_rule(&mut self, rule: &Rule) -> Result<(), Self::Error> {
        walk_rule(self, rule)
    }

    fn visit_input(&mut self, input: &InputClause) -> Result<(), Self::Error> {
        walk_input(self, input)
    }

    fn visit_select_step(&mut self, step: &SelectStep) -> Result<(), Self::Error> {
        walk_select_step(self, step)
    }

    fn visit_trigger(&mut self, _trigger: &TriggerClause) -> Result<(), Self::Error> {
        Ok(())
    }

    fn visit_require(&mut self, require: &RequireClause) -> Result<(), Self::Error> {
        walk_require(self, require)
    }

    fn visit_enforce(&mut self, _enforce: &EnforceClause) -> Result<(), Self::Error> {
        Ok(())
    }

    // A required hook lets type inference return a type without a side-channel result stack.
    fn visit_expression(
        &mut self,
        expression: &Expression,
    ) -> Result<Self::ExpressionOutput, Self::Error>;

    fn visit_identifier(&mut self, _identifier: &Identifier) -> Result<(), Self::Error> {
        Ok(())
    }

    fn visit_literal(&mut self, _literal: &Literal) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub fn walk_program<V: Visitor + ?Sized>(
    visitor: &mut V,
    program: &Program,
) -> Result<(), V::Error> {
    for rule in &program.rules {
        visitor.visit_rule(rule)?;
    }
    Ok(())
}

pub fn walk_rule<V: Visitor + ?Sized>(visitor: &mut V, rule: &Rule) -> Result<(), V::Error> {
    visitor.visit_identifier(&rule.name)?;
    for trigger in &rule.triggers {
        visitor.visit_trigger(trigger)?;
    }
    // Input declarations are visited before requirements, independently of source order.
    for input in &rule.inputs {
        visitor.visit_input(input)?;
    }
    for require in &rule.requires {
        visitor.visit_require(require)?;
    }
    visitor.visit_enforce(&rule.enforce)
}

pub fn walk_input<V: Visitor + ?Sized>(
    visitor: &mut V,
    input: &InputClause,
) -> Result<(), V::Error> {
    visitor.visit_identifier(&input.name)?;
    for step in &input.select {
        visitor.visit_select_step(step)?;
    }
    Ok(())
}

pub fn walk_select_step<V: Visitor + ?Sized>(
    visitor: &mut V,
    step: &SelectStep,
) -> Result<(), V::Error> {
    match step {
        SelectStep::Field(field) => visitor.visit_identifier(field),
        SelectStep::Filter { field, value } => {
            visitor.visit_identifier(field)?;
            visitor.visit_literal(value)
        }
    }
}

pub fn walk_require<V: Visitor + ?Sized>(
    visitor: &mut V,
    require: &RequireClause,
) -> Result<(), V::Error> {
    visitor.visit_expression(&require.expression)?;
    Ok(())
}

/// Walk children while discarding their expression results, for structural analyses.
pub fn walk_expression<V: Visitor + ?Sized>(
    visitor: &mut V,
    expression: &Expression,
) -> Result<(), V::Error> {
    match expression {
        Expression::Literal(literal) => visitor.visit_literal(literal)?,
        Expression::Identifier(identifier) => visitor.visit_identifier(identifier)?,
        Expression::Field { base, field } => {
            visitor.visit_expression(base)?;
            visitor.visit_identifier(field)?;
        }
        Expression::Call { callee, arguments } => {
            visitor.visit_expression(callee)?;
            for argument in arguments {
                visitor.visit_expression(argument)?;
            }
        }
        Expression::Unary { operand, .. } => {
            visitor.visit_expression(operand)?;
        }
        Expression::Binary { left, right, .. } => {
            visitor.visit_expression(left)?;
            visitor.visit_expression(right)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    /// Records traversal order and stops at a designated identifier.
    #[derive(Default)]
    struct Recorder {
        /// The hooks reached before traversal completes or fails.
        events: Vec<String>,
    }

    impl Visitor for Recorder {
        type Error = &'static str;
        type ExpressionOutput = ();

        fn visit_expression(&mut self, expression: &Expression) -> Result<(), Self::Error> {
            walk_expression(self, expression)
        }

        fn visit_identifier(&mut self, identifier: &Identifier) -> Result<(), Self::Error> {
            self.events.push(format!("id:{}", identifier.0));
            if identifier.0 == "stop_here" {
                return Err("stopped");
            }
            Ok(())
        }

        fn visit_literal(&mut self, _literal: &Literal) -> Result<(), Self::Error> {
            self.events.push("literal".into());
            Ok(())
        }

        fn visit_trigger(&mut self, _trigger: &TriggerClause) -> Result<(), Self::Error> {
            self.events.push("trigger".into());
            Ok(())
        }

        fn visit_enforce(&mut self, _enforce: &EnforceClause) -> Result<(), Self::Error> {
            self.events.push("enforce".into());
            Ok(())
        }
    }

    #[test]
    fn walks_clauses_selectors_and_every_expression_shape() {
        let program = parse(
            r#"
            rule r {
                require !ready && f(reading + 1, target.value);
                enforce deny;
                input number reading from "source" select items[id = "sensor"].value maxage 1;
                trigger always;
            }
        "#,
        )
        .unwrap();
        let mut visitor = Recorder::default();
        visitor.visit_program(&program).unwrap();
        assert_eq!(
            visitor.events,
            [
                "id:r",
                "trigger",
                "id:reading",
                "id:items",
                "id:id",
                "literal",
                "id:value",
                "id:ready",
                "id:f",
                "id:reading",
                "literal",
                "id:target",
                "id:value",
                "enforce",
            ]
        );
    }

    #[test]
    fn propagates_errors_without_visiting_remaining_children() {
        let program =
            parse("rule r { trigger always; require stop_here || false; enforce deny; }").unwrap();
        let mut visitor = Recorder::default();
        assert_eq!(visitor.visit_program(&program), Err("stopped"));
        assert_eq!(visitor.events, ["id:r", "trigger", "id:stop_here"]);
    }
}
