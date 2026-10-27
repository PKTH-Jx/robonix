use super::ast::{
    BinaryOperator as B, EnforceBehavior, EnforceClause, Expression as E, Identifier, InputType,
    Literal, SelectStep, TriggerClause, UnaryOperator as U,
};
use super::{Error, ErrorKind, parse};

fn condition(source: &str) -> Result<E, Error> {
    let program = parse(&format!(
        "rule test {{ trigger always; require {source}; enforce deny; }}"
    ))?;
    Ok(program
        .rules
        .into_iter()
        .next()
        .unwrap()
        .requires
        .remove(0)
        .expression)
}

fn name(value: &str) -> E {
    E::Identifier(Identifier(value.to_owned()))
}

fn number(value: &str) -> E {
    E::Literal(Literal::Number(value.parse().unwrap()))
}

fn binary(operator: B, left: E, right: E) -> E {
    E::Binary {
        operator,
        left: Box::new(left),
        right: Box::new(right),
    }
}

#[test]
fn parses_test_srdl() {
    let program = parse(include_str!("../../test.srdl")).unwrap();
    println!("{program:#?}");
    assert_eq!(program.rules.len(), 2);
    for (rule, limit) in program.rules.iter().zip(["10", "20"]) {
        assert_eq!(rule.name, Identifier(format!("speed_not_exceed_{limit}")));
        assert_eq!(rule.inputs.len(), 1);
        let input = &rule.inputs[0];
        assert_eq!(input.data_type, InputType::Number);
        assert_eq!(input.name, Identifier("velocity".into()));
        assert_eq!(input.capability, "robonix/system/soma/get_health");
        assert_eq!(input.maxage, 0.1);
        assert_eq!(
            input.select,
            vec![
                SelectStep::Field(Identifier("snapshot".into())),
                SelectStep::Field(Identifier("actuators".into())),
                SelectStep::Filter {
                    field: Identifier("component_id".into()),
                    value: Literal::String("left_wheel".into()),
                },
                SelectStep::Field(Identifier("velocity".into())),
                SelectStep::Field(Identifier("value".into())),
            ]
        );
        assert_eq!(rule.triggers.len(), 1);
        assert_eq!(
            rule.triggers[0],
            if limit == "10" {
                TriggerClause::Capability("robonix/primitive/chassis/move".into())
            } else {
                TriggerClause::Always
            }
        );
        assert_eq!(rule.requires.len(), 1);
        assert_eq!(
            rule.requires[0].expression,
            binary(B::Lte, name("velocity"), number(limit))
        );
        assert_eq!(rule.enforce.behavior, EnforceBehavior::Deny);
        assert_eq!(
            rule.enforce.report.as_deref(),
            if limit == "10" {
                Some("Velocity exceeds the configured limit.")
            } else {
                None
            }
        );
    }
}

#[test]
fn parses_empty_programs() {
    assert!(parse("").unwrap().rules.is_empty());
    assert!(parse("# no active rules\n").unwrap().rules.is_empty());
}

#[test]
fn preserves_precedence_and_left_associativity() {
    assert_eq!(
        condition("a || b && !c = -2 + 3 * 4").unwrap(),
        binary(
            B::Or,
            name("a"),
            binary(
                B::And,
                name("b"),
                binary(
                    B::Eq,
                    E::Unary {
                        operator: U::Not,
                        operand: Box::new(name("c"))
                    },
                    binary(
                        B::Add,
                        E::Unary {
                            operator: U::Negate,
                            operand: Box::new(number("2"))
                        },
                        binary(B::Multiply, number("3"), number("4"))
                    )
                )
            )
        )
    );
    assert_eq!(
        condition("a - b - c").unwrap(),
        binary(
            B::Subtract,
            binary(B::Subtract, name("a"), name("b")),
            name("c")
        )
    );
    assert_eq!(
        condition("(a + b) * c").unwrap(),
        binary(B::Multiply, binary(B::Add, name("a"), name("b")), name("c"))
    );
}

#[test]
fn parses_calls_fields_and_literals() {
    assert_eq!(
        condition(r#"clean(scene.target, "盘子\n\u0041", 2.5e-3, true, false, ready())"#).unwrap(),
        E::Call {
            callee: Box::new(name("clean")),
            arguments: vec![
                E::Field {
                    base: Box::new(name("scene")),
                    field: Identifier("target".into())
                },
                E::Literal(Literal::String("盘子\nA".into())),
                number("2.5e-3"),
                E::Literal(Literal::Boolean(true)),
                E::Literal(Literal::Boolean(false)),
                E::Call {
                    callee: Box::new(name("ready")),
                    arguments: vec![]
                },
            ],
        }
    );
    for source in [
        "soma.velocity",
        "vitals.battery",
        "capability.args.speed",
        "scene.target.position.x",
    ] {
        assert!(condition(source).is_ok(), "{source}");
    }
}

#[test]
fn parses_all_binary_operators() {
    for (symbol, operator) in [
        ("||", B::Or),
        ("&&", B::And),
        ("=", B::Eq),
        ("!=", B::Neq),
        ("<", B::Lt),
        ("<=", B::Lte),
        (">", B::Gt),
        (">=", B::Gte),
        ("+", B::Add),
        ("-", B::Subtract),
        ("*", B::Multiply),
        ("/", B::Divide),
        ("%", B::Remainder),
    ] {
        assert_eq!(
            condition(&format!("a {symbol} b")).unwrap(),
            binary(operator, name("a"), name("b"))
        );
    }
}

#[test]
fn rejects_invalid_expressions_and_partial_programs() {
    for source in [
        "",
        "a +",
        "a b",
        "a < b < c",
        "a = b != c",
        "(a",
        "a)",
        "f(,a)",
        "f(a,)",
        "f(a",
        "a.",
        "true &&",
        "1e9999",
        r#""bad\q""#,
        r#""unterminated"#,
    ] {
        assert!(condition(source).is_err(), "accepted {source:?}");
    }
    assert!(parse("rule broken {").is_err());
    let valid = "rule a { trigger always; require true; enforce deny; }";
    assert!(parse(&format!("{valid} garbage")).is_err());
    assert!(parse(&format!("{valid} rule broken {{")).is_err());
    assert_eq!(
        condition("a @ b").unwrap_err().kind,
        ErrorKind::InvalidToken
    );
}

#[test]
fn rejects_excessive_nesting_and_expression_size() {
    assert!(condition(&format!("{}a{}", "(".repeat(200), ")".repeat(200))).is_err());
    assert!(condition(&format!("{}a", "!".repeat(200))).is_err());
    assert!(condition(&vec!["a"; 600].join(" + ")).is_err());
    assert!(condition(&format!("a{}", ".b".repeat(600))).is_err());
}

#[test]
fn collects_multiple_inputs_triggers_and_requirements_in_any_order() {
    let program = parse(
        r#"
        rule limits {
            require velocity <= 0.5;
            enforce deny;
            input number velocity from "robonix/system/soma/get_health"
                select snapshot.actuators[component_id = "left_wheel"].velocity maxage 0.1;
            trigger capability "robonix/primitive/chassis/move";
            require motor_temp < 60;
            input number motor_temp from "robonix/system/soma/get_health"
                select snapshot.actuators[component_id = "left_wheel"].motor_temp maxage 2;
            trigger capability "robonix/primitive/quadruped/move";
        }
    "#,
    )
    .unwrap();
    let rule = &program.rules[0];
    assert_eq!(rule.inputs.len(), 2);
    assert_eq!(rule.inputs[0].name, Identifier("velocity".into()));
    assert_eq!(rule.inputs[1].data_type, InputType::Number);
    assert_eq!(rule.inputs[1].maxage, 2.0);
    assert_eq!(
        rule.triggers,
        vec![
            TriggerClause::Capability("robonix/primitive/chassis/move".into()),
            TriggerClause::Capability("robonix/primitive/quadruped/move".into()),
        ]
    );
    assert_eq!(rule.requires.len(), 2);
    assert_eq!(
        rule.requires[0].expression,
        binary(B::Lte, name("velocity"), number("0.5"))
    );
    assert_eq!(
        rule.requires[1].expression,
        binary(B::Lt, name("motor_temp"), number("60"))
    );
}

#[test]
fn accepts_all_statement_orders() {
    let clauses = [
        r#"input number velocity from "source" select value maxage 1;"#,
        "trigger always;",
        "require velocity < 1;",
        "enforce deny;",
    ];
    for first in 0..4 {
        for second in (0..4).filter(|i| *i != first) {
            for third in (0..4).filter(|i| *i != first && *i != second) {
                // The four distinct indices sum to 6, leaving exactly one final clause.
                let fourth = 6 - first - second - third;
                let body = [first, second, third, fourth]
                    .map(|i| clauses[i])
                    .join("\n");
                let program = parse(&format!("rule ordered {{ {body} }}")).unwrap();
                let rule = &program.rules[0];
                assert_eq!(rule.inputs.len(), 1);
                assert_eq!(rule.triggers.len(), 1);
                assert_eq!(rule.requires.len(), 1);
                assert_eq!(rule.enforce.behavior, EnforceBehavior::Deny);
                assert_eq!(rule.enforce.report, None);
            }
        }
    }
}

#[test]
fn parses_literal_array_filters() {
    for (filter, expected) in [
        (r#""wheel\u0041""#, Literal::String("wheelA".into())),
        ("true", Literal::Boolean(true)),
        ("false", Literal::Boolean(false)),
        ("2", Literal::Number(2.0)),
        ("-2", Literal::Number(-2.0)),
    ] {
        let program = parse(&format!(r#"
            rule custom {{
                input number soma from "custom/source" select readings[id = {filter}].value maxage 1e-2;
                trigger always;
                require soma > 0;
                enforce deny;
            }}
        "#)).unwrap();
        let input = &program.rules[0].inputs[0];
        assert_eq!(input.data_type, InputType::Number);
        assert_eq!(input.name, Identifier("soma".into()));
        assert_eq!(input.maxage, 0.01);
        assert_eq!(
            input.select,
            vec![
                SelectStep::Field(Identifier("readings".into())),
                SelectStep::Filter {
                    field: Identifier("id".into()),
                    value: expected
                },
                SelectStep::Field(Identifier("value".into())),
            ]
        );
    }
}

#[test]
fn rejects_invalid_input_declarations() {
    for declaration in [
        r#"input soma;"#,
        r#"input number v from "source" select value within 1;"#,
        r#"input number v from "source" select value max_age 1;"#,
        r#"input number v from "source" select value maxage 100ms;"#,
        r#"input number v from "source" select value maxage 1s;"#,
        r#"input number v from "source" select value maxage -1;"#,
        r#"input number v from "source" select value maxage 0;"#,
        r#"input number v from "source" select value maxage 1e9999;"#,
        r#"input number v from "source" select value maxage 1e-9999;"#,
        r#"input number v from source select value maxage 1;"#,
        r#"input number v from "source" select maxage 1;"#,
        r#"input number v from "source" select value. maxage 1;"#,
        r#"input number v from "source" select items[0] maxage 1;"#,
        r#"input number v from "source" select items[id != "a"] maxage 1;"#,
        r#"input number v from "source" select items[id = name] maxage 1;"#,
        r#"input number v from "source" select items[id = 1 + 2] maxage 1;"#,
        r#"input number v from "source" select items[id = "a" maxage 1;"#,
        r#"input number v from "source" maxage 1;"#,
        r#"input number v from "source" select value;"#,
    ] {
        let source =
            format!("rule invalid {{ {declaration} trigger always; require true; enforce deny; }}");
        assert!(parse(&source).is_err(), "accepted {declaration}");
    }
}

#[test]
fn rejects_missing_semicolons_duplicate_inputs_and_invalid_clause_counts() {
    for body in [
        "trigger always; check true; enforce deny;",
        "trigger always; require true; action stop;",
        "trigger always require true; enforce deny;",
        "trigger always; require true enforce deny;",
        "trigger always; require true; enforce deny",
        "trigger always; require true; enforce deny;;",
        "trigger always; require true; enforce deny; enforce deny;",
        "trigger always; require true;",
        "require true; enforce deny;",
        "trigger always; enforce deny;",
        "",
        r#"input number v from "source" select value maxage 1;
           input number v from "source" select value maxage 2;
           trigger always; require true; enforce deny;"#,
    ] {
        assert!(
            parse(&format!("rule invalid {{ {body} }}")).is_err(),
            "accepted {body}"
        );
    }
}

#[test]
fn parses_mixed_and_repeated_trigger_alternatives() {
    let program = parse(
        r#"
        rule alternatives {
            trigger capability "robonix/primitive/chassis/move";
            require true;
            trigger always;
            enforce deny;
            trigger capability "robonix/primitive/quadruped/move";
            trigger always;
        }
    "#,
    )
    .unwrap();
    assert_eq!(
        program.rules[0].triggers,
        vec![
            TriggerClause::Capability("robonix/primitive/chassis/move".into()),
            TriggerClause::Always,
            TriggerClause::Capability("robonix/primitive/quadruped/move".into()),
            TriggerClause::Always,
        ]
    );
}

#[test]
fn rejects_invalid_trigger_syntax() {
    for trigger in [
        "trigger;",
        "trigger move;",
        "trigger always_extra;",
        "trigger always() ;",
        r#"trigger "always";"#,
        r#"trigger always "source";"#,
        "trigger capability;",
        "trigger capability source;",
        "trigger capability 1;",
        "trigger capability always;",
        r#"trigger capability "source" extra;"#,
        r#"trigger capability "source" require true;"#,
        "trigger always require true;",
    ] {
        let source = format!("rule invalid {{ {trigger} require true; enforce deny; }}");
        assert!(parse(&source).is_err(), "accepted {trigger}");
    }
}

#[test]
fn trigger_keywords_remain_usable_as_identifiers() {
    let program = parse(
        r#"
        rule always {
            input number capability from "source" select always.capability maxage 1;
            require capability > always;
            trigger always;
            enforce deny;
        }
    "#,
    )
    .unwrap();
    let rule = &program.rules[0];
    assert_eq!(rule.name, Identifier("always".into()));
    assert_eq!(rule.inputs[0].name, Identifier("capability".into()));
    assert_eq!(
        rule.inputs[0].select,
        vec![
            SelectStep::Field(Identifier("always".into())),
            SelectStep::Field(Identifier("capability".into())),
        ]
    );
    assert_eq!(
        rule.requires[0].expression,
        binary(B::Gt, name("capability"), name("always"))
    );
    assert_eq!(rule.triggers, vec![TriggerClause::Always]);
}

#[test]
fn parses_enforcement_behaviors_with_optional_reports() {
    for (keyword, behavior) in [
        ("allow", EnforceBehavior::Allow),
        ("deny", EnforceBehavior::Deny),
        ("pend", EnforceBehavior::Pend),
    ] {
        for (suffix, expected) in [
            ("", None),
            (r#" report """#, Some("")),
            (r#" report "等待\n\"用户\"\u0041""#, Some("等待\n\"用户\"A")),
        ] {
            let program = parse(&format!(
                "rule response {{ enforce {keyword}{suffix}; trigger always; require false; }}"
            ))
            .unwrap();
            assert_eq!(
                program.rules[0].enforce,
                EnforceClause {
                    behavior,
                    report: expected.map(str::to_owned),
                }
            );
        }
    }
}

#[test]
fn rejects_invalid_enforcement_behaviors_and_reports() {
    for clause in [
        "enforce;",
        "enforce stop;",
        "enforce deny_extra;",
        "enforce allow deny;",
        r#"enforce "allow";"#,
        "enforce deny report;",
        "enforce deny report 1;",
        "enforce deny report message;",
        r#"enforce report "reason" deny;"#,
        r#"enforce deny "reason";"#,
        r#"enforce deny report "a" report "b";"#,
        r#"enforce deny report "reason""#,
        r#"enforce deny report "\uD800";"#,
    ] {
        let source = format!("rule invalid {{ trigger always; require false; {clause} }}");
        assert!(parse(&source).is_err(), "accepted {clause}");
    }
}

#[test]
fn enforcement_keywords_remain_usable_as_identifiers() {
    let program = parse(
        r#"
        rule report {
            input number allow from "source" select deny[pend = true].report maxage 1;
            trigger always;
            require allow > deny;
            enforce pend report "Wait for user intervention.";
        }
    "#,
    )
    .unwrap();
    let rule = &program.rules[0];
    assert_eq!(rule.name, Identifier("report".into()));
    assert_eq!(rule.inputs[0].name, Identifier("allow".into()));
    assert_eq!(
        rule.inputs[0].select,
        vec![
            SelectStep::Field(Identifier("deny".into())),
            SelectStep::Filter {
                field: Identifier("pend".into()),
                value: Literal::Boolean(true)
            },
            SelectStep::Field(Identifier("report".into())),
        ]
    );
    assert_eq!(
        rule.requires[0].expression,
        binary(B::Gt, name("allow"), name("deny"))
    );
    assert_eq!(rule.enforce.behavior, EnforceBehavior::Pend);
}

#[test]
fn selects_keyword_named_fields_at_every_path_position() {
    for keyword in [
        "rule",
        "input",
        "trigger",
        "require",
        "enforce",
        "from",
        "select",
        "maxage",
        "always",
        "capability",
        "allow",
        "deny",
        "pend",
        "report",
        "true",
        "false",
    ] {
        let program = parse(&format!(
            r#"
            rule fields {{
                trigger always;
                input number reading from "source"
                    select {keyword}.{keyword}[{keyword} = true].{keyword} maxage 0.1;
                require reading > 0;
                enforce deny;
            }}
        "#
        ))
        .unwrap();
        let input = &program.rules[0].inputs[0];
        assert_eq!(
            input.select,
            vec![
                SelectStep::Field(Identifier(keyword.into())),
                SelectStep::Field(Identifier(keyword.into())),
                SelectStep::Filter {
                    field: Identifier(keyword.into()),
                    value: Literal::Boolean(true),
                },
                SelectStep::Field(Identifier(keyword.into())),
            ]
        );
        assert_eq!(input.maxage, 0.1);
    }
}

#[test]
fn select_keyword_fields_preserve_delimiters_and_literal_rules() {
    for path in ["maxage", "snapshot.maxage", "items[maxage = false].maxage"] {
        assert!(
            parse(&format!(
                r#"
            rule valid {{
                input number reading from "source" select {path} maxage 1;
                trigger always; require reading > 0; enforce deny;
            }}
        "#
            ))
            .is_ok(),
            "rejected {path}"
        );
    }
    for selection in [
        "snapshot.input 1",
        "snapshot.input maxage",
        "snapshot.input maxage 1ms",
        "snapshot.1 maxage 1",
        r#"snapshot."input" maxage 1"#,
        "items[require = input].value maxage 1",
        "items[require = maxage].value maxage 1",
        "items[1 = true].value maxage 1",
        "snapshot.input require true",
    ] {
        assert!(
            parse(&format!(
                r#"
            rule invalid {{
                input number reading from "source" select {selection};
                trigger always; require reading > 0; enforce deny;
            }}
        "#
            ))
            .is_err(),
            "accepted {selection}"
        );
    }
    assert!(parse("rule input { trigger always; require true; enforce deny; }").is_err());
}

#[test]
fn numeric_spellings_share_one_value_type_in_all_contexts() {
    for (spelling, expected) in [
        ("1", 1.0),
        ("1.0", 1.0),
        ("01", 1.0),
        ("01.0", 1.0),
        ("1e0", 1.0),
        ("01e+0", 1.0),
        ("1.5", 1.5),
        ("01.50", 1.5),
        ("15e-1", 1.5),
        ("1.5E+1", 15.0),
    ] {
        let program = parse(&format!(
            r#"
            rule numbers {{
                trigger always;
                input number reading from "source"
                    select readings[id = {spelling}].value maxage {spelling};
                require reading <= {spelling};
                enforce deny;
            }}
        "#
        ))
        .unwrap();
        let rule = &program.rules[0];
        assert_eq!(rule.inputs[0].maxage, expected);
        assert_eq!(
            rule.inputs[0].select[1],
            SelectStep::Filter {
                field: Identifier("id".into()),
                value: Literal::Number(expected),
            }
        );
        assert_eq!(
            rule.requires[0].expression,
            binary(
                B::Lte,
                name("reading"),
                E::Literal(Literal::Number(expected))
            )
        );
    }
    let program = parse(
        r#"
        rule signed {
            input number reading from "source" select readings[id = -01.5].value maxage 1;
            trigger always;
            require reading >= -01.5;
            enforce deny;
        }
    "#,
    )
    .unwrap();
    assert_eq!(
        program.rules[0].inputs[0].select[1],
        SelectStep::Filter {
            field: Identifier("id".into()),
            value: Literal::Number(-1.5),
        }
    );
    assert_eq!(
        program.rules[0].requires[0].expression,
        binary(
            B::Gte,
            name("reading"),
            E::Unary {
                operator: U::Negate,
                operand: Box::new(number("1.5"))
            }
        )
    );
}

#[test]
fn rejects_invalid_numbers_consistently_in_all_contexts() {
    for spelling in ["1e9999", "1e", "1e+", "0x10", "1_000", ".5", "1.", "1.2.3"] {
        assert!(
            condition(spelling).is_err(),
            "accepted expression {spelling}"
        );
        for (selection, maxage) in [
            (format!("readings[id = {spelling}].value"), "1"),
            ("value".into(), spelling),
        ] {
            let source = format!(
                r#"
                rule invalid {{
                    trigger always;
                    input number reading from "source" select {selection} maxage {maxage};
                    require true;
                    enforce deny;
                }}
            "#
            );
            assert!(
                parse(&source).is_err(),
                "accepted select {selection} maxage {maxage}"
            );
        }
    }
}

#[test]
fn reports_the_original_error_inside_a_later_rule() {
    let source = "rule first { trigger always; require true; enforce deny; }\nrule second {\n    trigger always\n    require true;\n    enforce deny;\n}";
    let error = parse(source).unwrap_err();
    let start = source.rfind("require").unwrap();
    assert_eq!(error.span, start..start + "require".len());
    assert_eq!((error.line, error.column), (4, 5));
    assert_eq!(
        error.kind,
        ErrorKind::Expected {
            expected: "';'".into(),
            found: "\"require\"".into(),
        }
    );
    assert_eq!(
        error.to_string(),
        "expected ';', found \"require\" at line 4, column 5"
    );
}

#[test]
fn reports_eof_and_unicode_locations() {
    let source = "rule r { trigger always; require true; enforce deny; # end\n  ";
    let error = parse(source).unwrap_err();
    assert_eq!(error.span, source.len()..source.len());
    assert_eq!((error.line, error.column), (2, 3));
    assert_eq!(
        error.kind,
        ErrorKind::Expected {
            expected: "'}'".into(),
            found: "end of input".into(),
        }
    );
    let error =
        parse("rule r { trigger always; require \"温度\" @ 1; enforce deny; }").unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidToken);
    assert_eq!(error.span, 42..43);
    assert_eq!((error.line, error.column), (1, 39));
}

#[test]
fn distinguishes_duplicate_and_missing_clauses() {
    let source = r#"rule r {
        input number x from "source" select value maxage 1;
        input number x from "source" select value maxage 1;
        trigger always; require true; enforce deny;
    }"#;
    let error = parse(source).unwrap_err();
    assert_eq!(error.kind, ErrorKind::DuplicateInput("x".into()));
    assert_eq!(error.span.start, source.rfind("input").unwrap());
    let source = "rule r { trigger always; require true; enforce deny; enforce allow; }";
    let error = parse(source).unwrap_err();
    assert_eq!(error.kind, ErrorKind::DuplicateEnforce);
    assert_eq!(error.span.start, source.rfind("enforce").unwrap());
    for (body, missing) in [
        ("require true; enforce deny;", "trigger"),
        ("trigger always; enforce deny;", "require"),
        ("trigger always; require true;", "enforce"),
    ] {
        let source = format!("rule r {{ {body} }}");
        let error = parse(&source).unwrap_err();
        assert_eq!(error.kind, ErrorKind::MissingClause(missing));
        assert_eq!(error.span, source.len() - 1..source.len());
    }
}

#[test]
fn retains_report_and_argument_failures() {
    let source = "rule r { trigger always; require true; enforce deny report 1; }";
    let error = parse(source).unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::Expected {
            expected: "a string literal".into(),
            found: "\"1\"".into(),
        }
    );
    assert_eq!(error.span.start, source.find('1').unwrap());
    let source = r#"rule r { trigger always; require true; enforce deny report "\uD800"; }"#;
    let error = parse(source).unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidString);
    assert_eq!(&source[error.span], r#""\uD800""#);
    for expression in ["f(a,)", "f(,a)", "f(a +)"] {
        let error = condition(expression).unwrap_err();
        assert!(
            matches!(error.kind, ErrorKind::Expected { ref expected, .. } if expected == "an expression")
        );
    }
}

#[test]
fn distinguishes_numeric_comparison_and_limit_errors() {
    let error = condition("1e9999").unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidNumber);
    assert_eq!(error.span.len(), "1e9999".len());
    let error = parse(
        r#"rule r {
        input number x from "source" select value maxage 0;
        trigger always; require true; enforce deny;
    }"#,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidMaxage);
    assert_eq!(error.span.len(), 1);
    assert_eq!(
        condition("a < b < c").unwrap_err().kind,
        ErrorKind::ChainedComparison
    );
    assert_eq!(
        condition(&vec!["a"; 600].join(" + ")).unwrap_err().kind,
        ErrorKind::ExpressionTooLong(1024)
    );
    assert_eq!(
        condition(&format!("{}a", "!".repeat(200)))
            .unwrap_err()
            .kind,
        ErrorKind::ExpressionTooDeep(128)
    );
}

#[test]
fn parses_only_basic_input_types() {
    let program = parse(
        r#"
        rule basic {
            trigger always;
            input number velocity from "source" select velocity maxage 0.1;
            input string mode from "source" select mode maxage 1;
            input boolean ready from "source" select ready maxage 1;
            require ready && mode = "active" && velocity <= 1;
            enforce deny;
        }
    "#,
    )
    .unwrap();
    assert_eq!(
        program.rules[0]
            .inputs
            .iter()
            .map(|input| input.data_type)
            .collect::<Vec<_>>(),
        vec![InputType::Number, InputType::String, InputType::Boolean]
    );
    for data_type in [
        "length",
        "time",
        "speed",
        "temperature",
        "battery",
        "force",
        "integer",
        "float",
        "bool",
        "Number",
    ] {
        let source = format!(
            r#"rule invalid {{
            trigger always;
            input {data_type} reading from "source" select value maxage 1;
            require true;
            enforce deny;
        }}"#
        );
        let error = parse(&source).unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::Expected {
                expected: "'number', 'string', or 'boolean'".into(),
                found: format!("{data_type:?}"),
            }
        );
        assert_eq!(&source[error.span], data_type);
    }
}
