use super::ast::{
    BinaryOperator as B, EnforceBehavior, EnforceClause, Expression as E, Identifier, InputField,
    InputType, Literal, Rule, SelectStep, TriggerTarget, TriggerTiming, UnaryOperator as U,
};
use super::{Error, ErrorKind, parse};

fn rule(body: &str) -> Result<Rule, Error> {
    Ok(parse(&format!("rule test {{ {body} }}"))?.rules.remove(0))
}

fn input(selection: &str, maxage: &str) -> Result<InputField, Error> {
    Ok(rule(&format!(
        r#"input from "source" select number reading = {selection} maxage {maxage};
        require true; enforce deny;"#
    ))?
    .inputs
    .remove(0)
    .fields
    .remove(0))
}

fn condition(source: &str) -> Result<E, Error> {
    Ok(rule(&format!("require {source}; enforce deny;"))?
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
    assert_eq!(program.rules.len(), 2);
    for (rule, limit) in program.rules.iter().zip(["10", "20"]) {
        assert_eq!(rule.name.0, format!("speed_not_exceed_{limit}"));
        assert_eq!(rule.inputs.len(), 1);
        let input = &rule.inputs[0];
        assert_eq!(input.fields[0].name.0, "velocity");
        assert_eq!(input.capability, "robonix/system/soma/get_health");
        assert_eq!(input.fields[0].maxage, Some(0.1));
        assert_eq!(
            rule.requires[0].expression,
            binary(B::Lte, name("velocity"), number(limit))
        );
        assert_eq!(rule.enforce.behavior, EnforceBehavior::Deny);
    }
    assert!(matches!(
        program.triggers[0].capabilities[0],
        TriggerTarget::Capability(_)
    ));
    assert_eq!(
        program.triggers[0].rules,
        [Identifier("speed_not_exceed_10".into())]
    );
    assert_eq!(program.triggers[1].capabilities, [TriggerTarget::Always]);
    assert_eq!(
        program.triggers[1].rules,
        [Identifier("speed_not_exceed_20".into())]
    );
    assert!(parse("# no active rules\n").unwrap().rules.is_empty());
}

#[test]
fn parses_exclusions_and_rejects_malformed_declarations() {
    let program = parse(r#"exclude A B; exclude mutual "robot/A" "robot/B";"#).unwrap();
    assert_eq!(
        program.excludes,
        [
            super::ast::ExcludeClause {
                source: "A".into(),
                target: "B".into(),
                mutual: false
            },
            super::ast::ExcludeClause {
                source: "robot/A".into(),
                target: "robot/B".into(),
                mutual: true
            },
        ]
    );
    for source in [
        "exclude;",
        "exclude A;",
        "exclude A B",
        "exclude mutual A;",
        "exclude A B C;",
        "exclude [A] B;",
        "exclude mutual A [B];",
        "exclude 1 B;",
        "exclude A B;;",
        "rule r { exclude A B; require true; enforce deny; }",
    ] {
        assert!(parse(source).is_err(), "{source}");
    }
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
    let valid = "rule a { require true; enforce deny; }";
    assert!(parse(&format!("{valid} garbage")).is_err());
    assert!(parse(&format!("{valid} rule broken {{")).is_err());
    assert_eq!(
        condition("a @ b").unwrap_err().kind,
        ErrorKind::InvalidToken
    );
}

#[test]
fn collects_multiple_inputs_and_requirements_in_any_order() {
    let program = parse(
        r#"
        rule limits {
            require velocity <= 0.5;
            enforce deny;
            input from "robonix/system/soma/get_health" select number velocity = snapshot.actuators[component_id = "left_wheel"].velocity maxage 0.1;
            require motor_temp < 60;
            input from "robonix/system/soma/get_health" select number motor_temp = snapshot.actuators[component_id = "left_wheel"].motor_temp maxage 2;
        }
    "#,
    )
    .unwrap();
    let rule = &program.rules[0];
    assert_eq!(rule.inputs.len(), 2);
    assert_eq!(rule.inputs[0].fields[0].name, Identifier("velocity".into()));
    assert_eq!(rule.inputs[1].fields[0].data_type, InputType::Number);
    assert_eq!(rule.inputs[1].fields[0].maxage, Some(2.0));
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
fn accepts_statement_order_independently() {
    let clauses = [
        r#"input from "source" select number x = value maxage 1;"#,
        "require x > 0;",
        "enforce deny;",
    ];
    for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2]] {
        let parsed = rule(&order.map(|i| clauses[i]).join(" ")).unwrap();
        assert_eq!((parsed.inputs.len(), parsed.requires.len()), (1, 1));
    }
}

#[test]
fn groups_fields_with_independent_paths_and_maxage() {
    let parsed = rule(
        r#"require velocity > 0;
        input from "source" select number velocity = snapshot.velocity maxage 0.1,
            string label = snapshot.label maxage 2, boolean ready maxage 3;
        enforce deny;"#,
    )
    .unwrap();
    assert_eq!(parsed.inputs.len(), 1);
    let input = &parsed.inputs[0];
    assert_eq!(input.capability, "source");
    assert_eq!(
        input
            .fields
            .iter()
            .map(|field| (field.name.0.as_str(), field.maxage))
            .collect::<Vec<_>>(),
        [
            ("velocity", Some(0.1)),
            ("label", Some(2.0)),
            ("ready", Some(3.0))
        ]
    );
    assert_eq!(
        input.fields[0].select,
        [
            SelectStep::Field(Identifier("snapshot".into())),
            SelectStep::Field(Identifier("velocity".into()))
        ]
    );
    assert!(input.fields[2].select.is_empty());
    for declaration in [
        r#"input from "source" select;"#,
        r#"input from "source" select number x maxage 1,;"#,
        r#"input from "source" select number x = maxage 1;"#,
        r#"input from "source" select number x maxage 1, number y maxage;"#,
        r#"input from "source" select number x = value + 1 maxage 1;"#,
    ] {
        assert!(
            rule(&format!("{declaration} require true; enforce deny;")).is_err(),
            "{declaration}"
        );
    }
    for declarations in [
        r#"input from "source" select number x maxage 1, boolean x maxage 2;"#,
        r#"input from "a" select number x maxage 1; input from "b" select number x maxage 2;"#,
    ] {
        assert_eq!(
            rule(&format!("{declarations} require true; enforce deny;"))
                .unwrap_err()
                .kind,
            ErrorKind::DuplicateInput("x".into())
        );
    }
}

#[test]
fn accepts_optional_maxage_per_field() {
    let parsed = rule(
        r#"input from "source" select number first = reading,
            boolean bounded = ready maxage 2, string root;
        require true; enforce deny;"#,
    )
    .unwrap();
    let fields = &parsed.inputs[0].fields;
    assert_eq!(
        fields.iter().map(|field| field.maxage).collect::<Vec<_>>(),
        [None, Some(2.0), None]
    );
    assert_eq!(
        fields[0].select,
        [SelectStep::Field(Identifier("reading".into()))]
    );
    assert!(fields[2].select.is_empty());
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
        assert_eq!(
            input(&format!("readings[id = {filter}].value"), "1")
                .unwrap()
                .select,
            [
                SelectStep::Field(Identifier("readings".into())),
                SelectStep::Filter {
                    field: Identifier("id".into()),
                    value: expected
                },
                SelectStep::Field(Identifier("value".into()))
            ]
        );
    }
}

#[test]
fn rejects_invalid_input_declarations() {
    for declaration in [
        r#"input soma;"#,
        r#"input from "source" select number v = value within 1;"#,
        r#"input from "source" select number v = value max_age 1;"#,
        r#"input from "source" select number v = value maxage 100ms;"#,
        r#"input from "source" select number v = value maxage 1s;"#,
        r#"input from "source" select number v = value maxage -1;"#,
        r#"input from "source" select number v = value maxage 0;"#,
        r#"input from "source" select number v = value maxage 1e9999;"#,
        r#"input from "source" select number v = value maxage 1e-9999;"#,
        r#"input from source select number v = value maxage 1;"#,
        r#"input from "source" select number v = maxage 1;"#,
        r#"input from "source" select number v = value. maxage 1;"#,
        r#"input from "source" select number v = items[0] maxage 1;"#,
        r#"input from "source" select number v = items[id != "a"] maxage 1;"#,
        r#"input from "source" select number v = items[id = name] maxage 1;"#,
        r#"input from "source" select number v = items[id = 1 + 2] maxage 1;"#,
        r#"input from "source" select number v = items[id = "a" maxage 1;"#,
        r#"input number v from "source" maxage 1;"#,
    ] {
        let source = format!("rule invalid {{ {declaration} require true; enforce deny; }}");
        assert!(parse(&source).is_err(), "accepted {declaration}");
    }
}

#[test]
fn rejects_missing_semicolons_duplicate_inputs_and_invalid_clause_counts() {
    for body in [
        "check true; enforce deny;",
        "require true; action stop;",
        "require true enforce deny;",
        "require true; enforce deny",
        "require true; enforce deny;;",
        "require true; enforce deny; enforce deny;",
        "require true;",
        "enforce deny;",
        "",
        r#"input from "source" select number v = value maxage 1;
           input from "source" select number v = value maxage 2;
           require true; enforce deny;"#,
    ] {
        assert!(
            parse(&format!("rule invalid {{ {body} }}")).is_err(),
            "accepted {body}"
        );
    }
}

#[test]
fn parses_top_level_triggers_with_single_items_and_lists() {
    let program = parse(
        r#"
        trigger r1 before "robot/move";
        rule r1 { require true; enforce deny; }
        trigger [r1, r2] after capA;
        trigger r2 before ["robot/move", "robot/stop"];
        rule r2 { require true; enforce deny; }
        trigger [r2, r1] before ["robot/move", capB];
        trigger [r1] after [always];
        trigger r2 before "always";
    "#,
    )
    .unwrap();
    assert_eq!(program.rules.len(), 2);
    assert_eq!(program.triggers.len(), 6);
    assert_eq!(
        program
            .triggers
            .iter()
            .map(|t| t.timing)
            .collect::<Vec<_>>(),
        [
            TriggerTiming::Before,
            TriggerTiming::After,
            TriggerTiming::Before,
            TriggerTiming::Before,
            TriggerTiming::After,
            TriggerTiming::Before
        ]
    );
    for (trigger, (rules, capabilities)) in program.triggers.iter().zip([
        (
            vec!["r1"],
            vec![TriggerTarget::Capability("robot/move".into())],
        ),
        (
            vec!["r1", "r2"],
            vec![TriggerTarget::Capability("capA".into())],
        ),
        (
            vec!["r2"],
            vec![
                TriggerTarget::Capability("robot/move".into()),
                TriggerTarget::Capability("robot/stop".into()),
            ],
        ),
        (
            vec!["r2", "r1"],
            vec![
                TriggerTarget::Capability("robot/move".into()),
                TriggerTarget::Capability("capB".into()),
            ],
        ),
        (vec!["r1"], vec![TriggerTarget::Always]),
        (vec!["r2"], vec![TriggerTarget::Capability("always".into())]),
    ]) {
        assert_eq!(
            trigger
                .rules
                .iter()
                .map(|name| name.0.as_str())
                .collect::<Vec<_>>(),
            rules
        );
        assert_eq!(trigger.capabilities, capabilities);
    }
}

#[test]
fn rejects_invalid_trigger_syntax() {
    for trigger in [
        "trigger;",
        "trigger r;",
        "trigger always;",
        r#"trigger capability "source";"#,
        "trigger r @ capA;",
        "trigger r before;",
        "trigger r after;",
        "trigger r after [];",
        "trigger r after [capA,];",
        "trigger r before always",
        "trigger r always;",
        "trigger [] before capA;",
        "trigger r before [];",
        "trigger [r,] before capA;",
        "trigger r before [capA,];",
        "trigger [r r2] before capA;",
        "trigger r before [capA capB];",
        "trigger [[r]] before capA;",
        "trigger r before [[capA]];",
        "trigger [r before capA;",
        "trigger r before [capA;",
        "trigger r, r2 before capA;",
        "trigger r before capA, capB;",
        "trigger 1 before capA;",
        "trigger r before 1;",
        r#"trigger "r" before capA;"#,
        r#"trigger r before "bad\uD800";"#,
    ] {
        let source = format!("rule r {{ require true; enforce deny; }} {trigger}");
        assert!(parse(&source).is_err(), "accepted {trigger}");
    }
    for trigger in [
        "trigger always;",
        "trigger r before capA;",
        r#"trigger capability "capA";"#,
    ] {
        assert!(
            parse(&format!(
                "rule r {{ {trigger} require true; enforce deny; }}"
            ))
            .is_err()
        );
    }
}

#[test]
fn contextual_keywords_remain_usable_as_names() {
    for (rule_name, input_name, other) in [
        ("always", "capability", "always"),
        ("report", "allow", "deny"),
    ] {
        let program = parse(&format!(
            r#"rule {rule_name} {{
            input from "source" select number {input_name} = value maxage 1;
            require {input_name} > {other}; enforce deny;
        }}
        trigger {rule_name} before always;"#
        ))
        .unwrap();
        let rule = &program.rules[0];
        assert_eq!(rule.name.0, rule_name);
        assert_eq!(rule.inputs[0].fields[0].name.0, input_name);
        assert_eq!(
            rule.requires[0].expression,
            binary(B::Gt, name(input_name), name(other))
        );
    }
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
                "rule response {{ enforce {keyword}{suffix}; require false; }}"
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
        let source = format!("rule invalid {{ require false; {clause} }}");
        assert!(parse(&source).is_err(), "accepted {clause}");
    }
}

#[test]
fn selects_keyword_named_fields_at_every_path_position() {
    for keyword in [
        "exclude",
        "mutual",
        "rule",
        "input",
        "trigger",
        "require",
        "before",
        "after",
        "ensure",
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
        assert_eq!(
            input(
                &format!("{keyword}.{keyword}[{keyword} = true].{keyword}"),
                "0.1"
            )
            .unwrap()
            .select,
            [
                SelectStep::Field(Identifier(keyword.into())),
                SelectStep::Field(Identifier(keyword.into())),
                SelectStep::Filter {
                    field: Identifier(keyword.into()),
                    value: Literal::Boolean(true)
                },
                SelectStep::Field(Identifier(keyword.into()))
            ]
        );
    }
}

#[test]
fn select_keyword_fields_preserve_delimiters_and_literal_rules() {
    for path in ["maxage", "snapshot.maxage", "items[maxage = false].maxage"] {
        assert!(input(path, "1").is_ok(), "{path}");
    }
    for path in [
        "snapshot.1",
        r#"snapshot."input""#,
        "items[require = input].value",
        "items[require = maxage].value",
        "items[1 = true].value",
    ] {
        assert!(input(path, "1").is_err(), "{path}");
    }
    assert!(parse("rule input { require true; enforce deny; }").is_err());
}

#[test]
fn numeric_spellings_share_one_value_type_in_all_contexts() {
    for (spelling, expected) in [
        ("1", 1.0),
        ("1.0", 1.0),
        ("01", 1.0),
        ("1e0", 1.0),
        ("01e+0", 1.0),
        ("01.50", 1.5),
        ("15e-1", 1.5),
        ("1.5E+1", 15.0),
    ] {
        let input = input(&format!("readings[id = {spelling}].value"), spelling).unwrap();
        assert_eq!(input.maxage, Some(expected));
        assert!(
            matches!(input.select[1], SelectStep::Filter { value: Literal::Number(value), .. } if value == expected)
        );
        assert_eq!(
            condition(spelling).unwrap(),
            E::Literal(Literal::Number(expected))
        );
    }
    assert!(matches!(
        input("readings[id = -01.5].value", "1").unwrap().select[1],
        SelectStep::Filter {
            value: Literal::Number(-1.5),
            ..
        }
    ));
    assert_eq!(
        condition("-01.5").unwrap(),
        E::Unary {
            operator: U::Negate,
            operand: Box::new(number("1.5"))
        }
    );
}

#[test]
fn rejects_invalid_numbers_consistently_in_all_contexts() {
    for spelling in ["1e9999", "1e", "1e+", "0x10", "1_000", ".5", "1.", "1.2.3"] {
        assert!(condition(spelling).is_err(), "expression {spelling}");
        assert!(
            input(&format!("readings[id = {spelling}].value"), "1").is_err(),
            "filter {spelling}"
        );
        assert!(input("value", spelling).is_err(), "maxage {spelling}");
    }
}

#[test]
fn reports_the_original_error_inside_a_later_rule() {
    let source = "rule first { require true; enforce deny; }\nrule second {\n    enforce deny\n    require true;\n}";
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
    let source = "rule r { require true; enforce deny; # end\n  ";
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
    let error = parse("rule r { require \"温度\" ? 1; enforce deny; }").unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidToken);
    assert_eq!(error.span, 26..27);
    assert_eq!((error.line, error.column), (1, 23));
}

#[test]
fn distinguishes_duplicate_and_missing_clauses() {
    let source = r#"rule r {
        input from "source" select number x = value maxage 1;
        input from "source" select number x = value maxage 1;
        require true; enforce deny;
    }"#;
    let error = parse(source).unwrap_err();
    assert_eq!(error.kind, ErrorKind::DuplicateInput("x".into()));
    assert_eq!(error.span.start, source.rfind("input").unwrap());
    let source = "rule r { require true; enforce deny; enforce allow; }";
    let error = parse(source).unwrap_err();
    assert_eq!(error.kind, ErrorKind::DuplicateEnforce);
    assert_eq!(error.span.start, source.rfind("enforce").unwrap());
    for (body, missing) in [("enforce deny;", "require"), ("require true;", "enforce")] {
        let source = format!("rule r {{ {body} }}");
        let error = parse(&source).unwrap_err();
        assert_eq!(error.kind, ErrorKind::MissingClause(missing));
        assert_eq!(error.span, source.len() - 1..source.len());
    }
}

#[test]
fn retains_report_and_argument_failures() {
    let source = "rule r { require true; enforce deny report 1; }";
    let error = parse(source).unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::Expected {
            expected: "a string literal".into(),
            found: "\"1\"".into(),
        }
    );
    assert_eq!(error.span.start, source.find('1').unwrap());
    let source = r#"rule r { require true; enforce deny report "\uD800"; }"#;
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
        input from "source" select number x = value maxage 0;
        require true; enforce deny;
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
fn rejects_removed_ensure_clauses() {
    assert!(rule("require true; ensure false; enforce deny;").is_err());
    assert!(rule("ensure true; enforce deny;").is_err());
}

#[test]
fn parses_only_basic_input_types() {
    let program = parse(
        r#"
        rule basic {
            input from "source" select number velocity = velocity maxage 0.1,
                string mode = mode maxage 1, boolean ready = ready maxage 1;
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
            .flat_map(|input| input.fields.iter())
            .map(|field| field.data_type)
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
            input from "source" select {data_type} reading = value maxage 1;
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
#[test]
fn parses_context_bindings_without_acquisition_or_freshness() {
    let program = parse(
        r#"rule r {
        require user_id = "alice" && ready && count > 0;
        context string user_id = identity.user_id;
        context boolean ready = identity.context.true;
        context number count = arguments.count;
        input from "source" select number context = context maxage 1;
        enforce deny;
    }"#,
    )
    .unwrap();
    let rule = &program.rules[0];
    assert_eq!(
        rule.contexts
            .iter()
            .map(|context| context.data_type)
            .collect::<Vec<_>>(),
        [InputType::String, InputType::Boolean, InputType::Number]
    );
    assert_eq!(rule.contexts[0].name.0, "user_id");
    assert_eq!(
        rule.contexts[0]
            .path
            .iter()
            .map(|field| field.0.as_str())
            .collect::<Vec<_>>(),
        ["identity", "user_id"]
    );
    assert_eq!(
        rule.contexts[1]
            .path
            .iter()
            .map(|field| field.0.as_str())
            .collect::<Vec<_>>(),
        ["identity", "context", "true"]
    );
    assert_eq!(
        rule.inputs[0].fields[0].select,
        [SelectStep::Field(Identifier("context".into()))]
    );
    for clause in [
        "context string user = identity.user_id maxage 1;",
        "context string user = identity[role = true];",
        "context string user = identity.;",
        "context string user = ;",
        "context string user identity.user_id;",
        "context string user = identity.user_id",
        "context length user = identity.user_id;",
    ] {
        assert!(
            parse(&format!(
                "rule r {{ {clause} require true; enforce deny; }}"
            ))
            .is_err(),
            "{clause}"
        );
    }
}
