use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::GLOBAL_ENV;

use super::*;

#[test]
fn type_names_are_literals_and_type_returns_types() {
    let mut engine = engine();
    let mut env = Env::new();
    for name in ["bool", "int", "float", "str", "list", "dict", "func", "type"] {
        let expected = EvalValue::Type(EvalType::from_name(name).unwrap());
        env.insert(name, EvalValue::Int(99));
        let ast = Parser::new(Tokenizer::new(name).tokenize().unwrap()).parse().unwrap();
        assert_eq!(ast, Expr::Literal(expected.clone()));
        assert_eq!(engine.evaluate(name, &env).unwrap(), expected);
        assert_eq!(engine.evaluate(&format!("${name}"), &env).unwrap(), expected);
        assert_eq!(engine.evalexpr(&Expr::Variable(name.into()), &env).unwrap(), expected);
        assert_eq!(Expr::try_from(ExprSerializer::Variable(name.into())).unwrap(), ast);
    }
    for (source, expected) in [
        ("type(null)", EvalType::Null), ("type(false)", EvalType::Bool),
        ("type(1)", EvalType::Int), ("type(1.0)", EvalType::Float),
        ("type('a')", EvalType::Str), ("type([])", EvalType::List),
        ("type({})", EvalType::Dict), ("type(fn -> 1)", EvalType::Func),
        ("type(sum)", EvalType::Func), ("type(int)", EvalType::Type),
        ("type(type)", EvalType::Type), ("type(type(null))", EvalType::Type),
    ] {
        assert_eq!(engine.evaluate(source, &env).unwrap(), EvalValue::Type(expected), "{source}");
    }
    assert_eq!(engine.evaluate("null", &env).unwrap(), EvalValue::Null);
    assert_eq!(engine.evaluate("None", &env).unwrap(), EvalValue::Null);
    assert_eq!(engine.evaluate("type(1) == 'int'", &env).unwrap(), EvalValue::Bool(false));
    assert_eq!(engine.format("$int ${int} $type".into(), &env).unwrap().as_ref(), "int int type");
    let mut bare_engine = FormulaEngine::new(Some(1)).with_globals(Env::new());
    assert_eq!(bare_engine.evaluate("type(int('12')) == int", &Env::new()).unwrap(), EvalValue::Bool(true));
}

#[test]
fn reserved_types_cannot_be_bound_but_can_be_property_names() {
    use crate::err::SyntaxError;

    for name in ["bool", "int", "float", "str", "list", "dict", "func", "type"] {
        for source in [
            format!("let {name} = 1 for 0"), format!("fn {name} -> 0"),
            format!("let [*{name}] = [] for 0"), format!("[0 for {name} in []]"),
            format!("let ${name} = 1 for 0"),
        ] {
            let error = validate_formula_syntax(&source).unwrap_err();
            assert!(matches!(error.without_context(), Error::Syntax {
                kind: SyntaxError::InvalidBinding { .. }, ..
            }), "{source}: {error:#}");
        }
        assert_eq!(engine().evaluate(&format!("{{'{name}': 7}}.{name}"), &Env::new()).unwrap(), EvalValue::Int(7));
    }
}

#[test]
fn callable_types_convert_values_and_report_errors() {
    let mut engine = engine();
    for (source, expected) in [
        ("int('42')", "42"), ("float('2.5')", "2.5"), ("bool([])", "false"),
        ("str(int)", "'int'"), ("str('x')", "'\"x\"'"),
        ("list('aé')", "['a', 'é']"), ("list({'a': 1, 'b': 2})", "['a', 'b']"),
        ("list([1, 2])", "[1, 2]"), ("dict([])", "{}"),
        ("dict([['a', 1], ['a', 2], ['b', 3]])", "{'a': 2, 'b': 3}"),
        ("dict({'a': 1})", "{'a': 1}"), ("func(fn x -> x + 1)(2)", "3"),
        ("type(null)(null)", "null"), ("let convert = int for convert('7')", "7"),
        ("apply(int, ['7'])", "7"), ("{'convert': int}.convert('7')", "7"),
        ("{'convert': int}['convert']('7')", "7"),
        ("['1', '2'].map(fn x -> int(x))", "[1, 2]"),
        ("try(fn -> error('bad'), type)", "dict"),
    ] {
        let expected = engine.evaluate(expected, &Env::new()).unwrap();
        assert_eq!(engine.evaluate(source, &Env::new()).unwrap(), expected, "{source}");
    }
    for source in ["int([])", "list(1)", "dict([1])", "dict([[1, 2]])",
        "dict([['a']])", "func(int)", "type(null)(1)"] {
        let error = engine.evaluate(source, &Env::new()).unwrap_err();
        assert!(matches!(error.without_context(), Error::Type(_)), "{source}: {error:#}");
    }
    for source in ["int()", "int(1, 2)", "type()", "list()", "type(null)()"] {
        let error = engine.evaluate(source, &Env::new()).unwrap_err();
        assert!(matches!(error.without_context(), Error::Arity { .. }), "{source}: {error:#}");
    }
}

#[test]
fn types_preserve_identity_ordering_and_serialization() {
    let mut engine = engine();
    for kind in [EvalType::Null, EvalType::Bool, EvalType::Int, EvalType::Float,
        EvalType::Str, EvalType::List, EvalType::Dict, EvalType::Func, EvalType::Type] {
        let value = EvalValue::Type(kind);
        assert!(value.truthy());
        assert!(value.is_callable());
        assert_eq!(value.type_name().as_ref(), "type");
        assert!(value.strict_eq(&value.clone()));
        let json = serde_json::to_string(&value).unwrap();
        let restored: EvalValue = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, value);
        assert_eq!(engine.evaluate(&value.to_string(), &Env::new()).unwrap(), value);
        let ast = Expr::Literal(value);
        let saved = serde_json::to_string(&ExprSerializer::from(&ast)).unwrap();
        assert_eq!(Expr::try_from(serde_json::from_str::<ExprSerializer>(&saved).unwrap()).unwrap(), ast);
    }
    assert!(serde_json::from_str::<EvalValue>(r#"{"Type":"imaginary"}"#).is_err());
    assert_eq!(engine.evaluate("int === type(1) and int != float", &Env::new()).unwrap(), EvalValue::Bool(true));
    assert_eq!(engine.evaluate("[int, float, int].unique()", &Env::new()).unwrap(),
        EvalValue::Array(vec![EvalValue::Type(EvalType::Int), EvalValue::Type(EvalType::Float)].into()));
    assert_eq!(engine.evaluate("order(int, float)", &Env::new()).unwrap(), EvalValue::Int(-1));
    assert_eq!(engine.evaluate("order(sum, int)", &Env::new()).unwrap(), EvalValue::Int(-1));
}

#[test]
fn type_values_survive_closures_and_legacy_builtin_loading() {
    let mut engine = engine();
    let mut env = Env::new();
    env.insert("int", EvalValue::Int(99));
    let closure = engine.evaluate("fn x -> int(x)", &env).unwrap();
    let EvalValue::Func(function) = &closure else { panic!("expected function") };
    let Function::User(function) = function.as_ref() else { panic!("expected user function") };
    assert!(!function.captures.contains_key("int"));
    let closure = engine.evaluate("let convert = int for fn x -> convert(x)", &env).unwrap();
    let restored: EvalValue = serde_json::from_str(&serde_json::to_string(&closure).unwrap()).unwrap();
    assert_eq!(restored.call(&[EvalValue::String("42".into())], &mut engine, &Env::new()).unwrap(), EvalValue::Int(42));
    for name in ["int", "float", "str", "bool", "type"] {
        let function = Function::try_from(FunctionSerializer::Builtin(name.into())).unwrap();
        let actual = EvalValue::Func(Arc::new(function))
            .call(&[EvalValue::Int(2)], &mut engine, &Env::new()).unwrap();
        let expected = engine.evaluate(&format!("{name}(2)"), &Env::new()).unwrap();
        assert_eq!(actual, expected);
    }
    engine = engine.with_max_rec(0);
    let error = engine.evaluate("int(1)", &Env::new()).unwrap_err();
    assert!(matches!(error.without_context(), Error::RecursionLimit { .. }));
}

#[test]
fn jokers_are_expressions_but_cannot_be_evaluated() {
    let mut engine = engine();
    let mut env = Env::new();
    env.insert("_", EvalValue::Int(42));
    for source in [
        "_", "1 + _", "_ + 1", "-_", "not _", "[_]", "{'a': _}",
        "[1][_]", "[1][_:]", "_(1)", "(fn x -> x)(_)",
        "let _ = _ for 1", "let _ = 42 for _", "(fn _ -> _)(42)",
    ] {
        let ast = Parser::new(Tokenizer::new(source).tokenize().unwrap()).parse().unwrap();
        let rendered = ast.to_string();
        assert_eq!(Parser::new(Tokenizer::new(&rendered).tokenize().unwrap()).parse().unwrap(), ast);
        let json = serde_json::to_string(&ExprSerializer::from(&ast)).unwrap();
        let restored = Expr::try_from(serde_json::from_str::<ExprSerializer>(&json).unwrap()).unwrap();
        assert_eq!(restored, ast);
        let error = engine.evaluate(source, &env).unwrap_err();
        assert_eq!(error.without_context(), &Error::InvalidJoker, "{source}");
    }
    for (source, expected) in [
        ("_ if false else 1", EvalValue::Int(1)),
        ("1 if true else _", EvalValue::Int(1)),
        ("false and _", EvalValue::Bool(false)),
        ("true or _", EvalValue::Bool(true)),
        ("1 ?? _", EvalValue::Int(1)),
    ] {
        assert_eq!(engine.evaluate(source, &env).unwrap(), expected, "{source}");
    }
    assert_eq!(parse_next("_, suffix").unwrap(), (Expr::Joker, 1));
    let closure = engine.evaluate("fn -> _", &env).unwrap();
    let EvalValue::Func(function) = &closure else { panic!("expected a function") };
    let Function::User(function) = function.as_ref() else { panic!("expected a user function") };
    assert!(!function.captures.contains_key("_"));
    assert_eq!(closure.call(&[], &mut engine, &Env::new()).unwrap_err().without_context(),
        &Error::InvalidJoker);
}

#[test]
fn joker_patterns_preserve_serialization_and_discard_values() {
    for (pattern, saved) in [
        (BindingPattern::Joker, serde_json::json!("_")),
        (BindingPattern::RestJoker, serde_json::json!({"rest": "_"})),
        (BindingPattern::List(vec![BindingPattern::Joker, BindingPattern::RestJoker]),
            serde_json::json!(["_", {"rest": "_"}])),
    ] {
        assert_eq!(serde_json::to_value(&pattern).unwrap(), saved);
        assert_eq!(serde_json::from_value::<BindingPattern>(saved).unwrap(), pattern);
    }
    for (name, expected) in [("_", BindingPattern::Joker), ("*_", BindingPattern::RestJoker)] {
        assert_eq!(BindingPattern::from(name), expected);
        assert_eq!(BindingPattern::from(name.to_owned()), expected);
        assert_eq!(BindingPattern::from(Arc::<str>::from(name)), expected);
    }
    let mut engine = engine();
    for (source, expected) in [
        ("let [*_, last] = [1, 2, 3] for last", "3"),
        ("let [*_, last] = [1] for last", "1"),
        ("let [first, *_] = [1, 2, 3] for first", "1"),
        ("let [first, *_, last] = [1, 2] for [first, last]", "[1, 2]"),
        ("let [*_] = [] for exists('_')", "false"),
        ("let [*_] = [1, 2] for exists('_')", "false"),
        ("let _ = 1, _ = 2 for exists('_')", "false"),
        ("(fn _, _ -> exists('_'))(1, 2)", "false"),
        ("(fn [_, *_, last] -> last)([1, 2, 3, 4])", "4"),
        ("[last for *_, last in [[1], [1, 2, 3]]]", "[1, 3]"),
        ("[first for first, *_ in [[1], [2, 3, 4]]]", "[1, 2]"),
        ("[exists('_') for _ in [1, 2]]", "[false, false]"),
    ] {
        let ast = Parser::new(Tokenizer::new(source).tokenize().unwrap()).parse().unwrap();
        let json = serde_json::to_string(&ExprSerializer::from(&ast)).unwrap();
        let restored = Expr::try_from(serde_json::from_str::<ExprSerializer>(&json).unwrap()).unwrap();
        assert_eq!(restored, ast);
        let expected = engine.evaluate(expected, &Env::new()).unwrap();
        assert_eq!(engine.evalexpr(&restored, &Env::new()).unwrap(), expected, "{source}");
        assert_eq!(engine.evaluate(&restored.to_string(), &Env::new()).unwrap(), expected, "{source}");
    }
}

#[test]
fn joker_rest_patterns_obey_shape_and_syntax_rules() {
    use crate::err::SyntaxError;

    for source in [
        "let [*_, *_] = [] for 1", "fn [*_, *rest] -> 1",
        "[1 for *_, *_ in []]", "[1 for *_, *rest in []]",
        "[1 for *rest, *_ in []]",
    ] {
        for error in [validate_formula_syntax(source).unwrap_err(), parse_next(source).unwrap_err()] {
            assert!(matches!(error.without_context(), Error::Syntax {
                kind: SyntaxError::MultipleRestPatterns, ..
            }), "{source}: {error:#}");
        }
    }
    for source in ["let *_ = [] for 1", "fn *_ -> 1", "[1 for *_ in []]"] {
        assert!(validate_formula_syntax(source).is_err(), "{source}");
        assert!(parse_next(source).is_err(), "{source}");
    }
    for source in ["let [first, *_, last] = [1] for last", "(fn [_, *_] -> 1)([])"] {
        let error = engine().evaluate(source, &Env::new()).unwrap_err();
        assert!(matches!(error.without_context(), Error::InvalidArgument(_)), "{source}: {error:#}");
    }
}

#[test]
fn underscore_is_reserved_but_longer_identifiers_and_string_keys_work() {
    assert_eq!(Tokenizer::new("_ _name name_ __").tokenize().unwrap(), vec![
        Token::Joker, Token::Identifier("_name".into()),
        Token::Identifier("name_".into()), Token::Identifier("__".into()), Token::End,
    ]);
    assert!(validate_formula_syntax("{'_': 1}._").is_err());
    assert_eq!(engine().evaluate("{'_': 1}['_']", &Env::new()).unwrap(), EvalValue::Int(1));
    assert_eq!(engine().evaluate("let _name = 1, name_ = 2, __ = 3 for _name + name_ + __",
        &Env::new()).unwrap(), EvalValue::Int(6));
}

#[test]
fn rest_patterns_bind_prefixes_suffixes_and_nested_lists() {
    let mut engine = engine();
    let env = Env::new();
    for (source, expected) in [
        ("let [head, *tail] = [1, 2, 3] for [head, tail]", "[1, [2, 3]]"),
        ("let [head, *tail] = [1] for [head, tail]", "[1, []]"),
        ("let [*prefix, last] = [1, 2, 3] for [prefix, last]", "[[1, 2], 3]"),
        ("let [first, *middle, last] = [1, 2, 3, 4] for [first, middle, last]", "[1, [2, 3], 4]"),
        ("let [first, *middle, last] = [1, 2] for [first, middle, last]", "[1, [], 2]"),
        ("let [*all,] = [] for all", "[]"),
        ("let [*all] = [1, 2] for all", "[1, 2]"),
        ("let [[x, *xs], *rows] = [[1, 2, 3], [4]] for [x, xs, rows]", "[1, [2, 3], [[4]]]"),
        ("let [[*xs], [*ys]] = [[1, 2], [3]] for [xs, ys]", "[[1, 2], [3]]"),
        ("let _ = 99 for let [_, *_, last] = [1, 2, 3] for last", "3"),
        ("let [head, *tail] = [1, 2, 3], total = tail.sum() for head + total", "6"),
        ("(fn [first, *middle, last], offset -> first + middle.sum() + last + offset)([1, 2, 3, 4], 10)", "20"),
        ("(fn [*all] -> all)([])", "[]"),
    ] {
        let expected = engine.evaluate(expected, &env).unwrap();
        assert_eq!(engine.evaluate(source, &env).unwrap(), expected, "{source}");
    }
    assert!(env.get("tail").is_none());
}

#[test]
fn comprehension_patterns_support_nested_lists_and_rest() {
    let mut engine = engine();
    let env = Env::new();
    for (source, expected) in [
        ("[a + b for [a, b] in [[1, 2], [3, 4]]]", "[3, 7]"),
        ("[[head, tail] for [head, *tail] in [[1, 2, 3], [4]]]", "[[1, [2, 3]], [4, []]]"),
        ("[tail for head, *tail in [[1, 2, 3], [4]]]", "[[2, 3], []]"),
        ("[prefix for *prefix, last in [[1, 2, 3], [4]]]", "[[1, 2], []]"),
        ("[middle for first, *middle, last in [[1, 2, 3], [4, 5]]]", "[[2], []]"),
        ("[all for [*all] in [[], [1, 2]]]", "[[], [1, 2]]"),
        ("[x + y + tail.sum() for [x, [y, *tail]] in [[1, [2, 3, 4]]]]", "[10]"),
        ("[last for [_, *_, last] in [[1, 2, 3]]]", "[3]"),
        ("[head for [head, *tail] in [[1], [2, 3]] if tail]", "[2]"),
        ("{key: tail for [key, head, *tail] in [['a', 1, 2], ['b', 3]]}", "{'a': [2], 'b': []}"),
        ("{key: tail for key, [head, *tail] in {'a': [1, 2, 3], 'b': [4]}}", "{'a': [2, 3], 'b': []}"),
        ("[a + b for [a, b], _ in [[[1, 2], 99]]]", "[3]"),
        ("[key for key in {'a': 1, 'b': 2}]", "['a', 'b']"),
        ("[value for key, value in {'a': 1, 'b': 2}]", "[1, 2]"),
        ("[ch for ch in 'ab']", "['a', 'b']"),
        ("[1 for [] in [[], []]]", "[1, 1]"),
        ("[tail for [head, *tail] in []]", "[]"),
    ] {
        let expected = engine.evaluate(expected, &env).unwrap();
        assert_eq!(engine.evaluate(source, &env).unwrap(), expected, "{source}");
    }
}

#[test]
fn pattern_failures_report_shape_and_syntax_errors() {
    use crate::err::SyntaxError;

    for source in [
        "let [a, *rest, b] = [1] for rest",
        "let [a, b] = [1, 2, 3] for a",
        "(fn [a, *rest] -> rest)([])",
        "[a for [a, *rest] in [[]]]",
        "[a for a, b in [[1]]]",
        "[a for [a, [b, *rest]] in [[1, []]]]",
        "[a for [a, b] in [[1]] if false]",
    ] {
        let error = engine().evaluate(source, &Env::new()).unwrap_err();
        assert!(matches!(error.without_context(), Error::InvalidArgument(_)), "{source}: {error:#}");
    }
    for source in [
        "let [*rest] = 'abc' for rest",
        "(fn [*rest] -> rest)(1)",
        "[a for [a, *rest] in [1]]",
        "[a for a, *rest in [1]]",
    ] {
        let error = engine().evaluate(source, &Env::new()).unwrap_err();
        assert!(matches!(error.without_context(), Error::Type(_)), "{source}: {error:#}");
    }
    for source in [
        "let [*a, *b] = [] for a",
        "fn [*a, *b] -> a",
        "[a for [*a, *b] in []]",
        "[a for *a, *b in []]",
    ] {
        let error = validate_formula_syntax(source).unwrap_err();
        assert!(matches!(error.without_context(), Error::Syntax {
            kind: SyntaxError::MultipleRestPatterns, ..
        }), "{source}: {error:#}");
    }
    for source in [
        "let [a, *a] = [1] for a",
        "fn [a, *rest], rest -> a",
        "[a for [a, *rest], rest in []]",
        "[a for a, a in []]",
    ] {
        let error = validate_formula_syntax(source).unwrap_err();
        assert!(matches!(error.without_context(), Error::Syntax {
            kind: SyntaxError::DuplicateBinding { .. }, ..
        }), "{source}: {error:#}");
    }
    for source in [
        "let *rest = [] for rest", "fn *rest -> rest", "[rest for *rest in []]",
        "let [*] = [] for 1", "let [*[a]] = [] for a", "fn [*args] -> 1",
        "[1 for [*for] in []]", "let [a, *", "[a for [a, *rest] in",
    ] {
        assert!(validate_formula_syntax(source).is_err(), "{source}");
        assert!(parse_next(source).is_err(), "{source}");
    }
}

#[test]
fn pattern_bindings_preserve_closure_scope() {
    let mut engine = engine();
    let mut env = Env::new();
    env.insert("head", EvalValue::Int(100));
    env.insert("tail", EvalValue::Int(200));
    env.insert("offset", EvalValue::Int(10));
    env.insert("rows", engine.evaluate("[[1, 2, 3]]", &Env::new()).unwrap());
    let closure = engine.evaluate(
        "fn -> [head + tail.sum() + offset for [head, *tail] in rows]", &env,
    ).unwrap();
    let EvalValue::Func(function) = &closure else { panic!("expected a closure") };
    let Function::User(function) = function.as_ref() else { panic!("expected a user closure") };
    assert!(!function.captures.contains_key("head"));
    assert!(!function.captures.contains_key("tail"));
    assert!(function.captures.contains_key("rows"));
    assert!(function.captures.contains_key("offset"));
    assert_eq!(closure.call(&[], &mut engine, &Env::new()).unwrap(),
        EvalValue::Array(vec![EvalValue::Int(16)].into()));

    for (source, expected) in [
        ("let fs = [fn -> [head, tail] for [head, *tail] in [[1, 2], [3, 4, 5]]] for [fs[0](), fs[1]()]", "[[1, [2]], [3, [4, 5]]]"),
        ("(fn [head, *tail] -> fn -> [head, tail])([1, 2, 3])()", "[1, [2, 3]]"),
        ("let f = let [head, *tail] = [1, 2, 3] for fn -> tail for f()", "[2, 3]"),
        ("let tail = 99 for [[tail for [head, *tail] in [[1, 2]]], tail]", "[[[2]], 99]"),
    ] {
        let expected = engine.evaluate(expected, &env).unwrap();
        assert_eq!(engine.evaluate(source, &env).unwrap(), expected, "{source}");
    }
    assert_eq!(env.get("tail"), Some(&EvalValue::Int(200)));
}

#[test]
fn pattern_formatting_and_serialization_preserve_bindings() {
    for source in [
        "let [head, *tail, last] = [1, 2, 3] for tail",
        "(fn [head, [*tail]] -> tail)([1, [2, 3]])",
        "[tail for head, *tail in [[1, 2, 3]]]",
        "[prefix for *prefix, last in [[1, 2, 3]]]",
        "[tail for [head, [*tail]] in [[1, [2, 3]]]]",
        "{key: tail for key, [head, *tail] in {'a': [1, 2]}}",
    ] {
        let ast = Parser::new(Tokenizer::new(source).tokenize().unwrap()).parse().unwrap();
        let rendered = ast.to_string();
        assert_eq!(Parser::new(Tokenizer::new(&rendered).tokenize().unwrap()).parse().unwrap(), ast);
        let (partial, end) = parse_next(&format!("{source}, @suffix")).unwrap();
        assert_eq!(partial, ast);
        assert_eq!(end, source.chars().count());
        let json = serde_json::to_string(&ExprSerializer::from(&ast)).unwrap();
        let restored = Expr::try_from(serde_json::from_str::<ExprSerializer>(&json).unwrap()).unwrap();
        assert_eq!(restored, ast);
        let mut engine = engine();
        assert_eq!(engine.evaluate(&restored.to_string(), &Env::new()).unwrap(),
            engine.evaluate(source, &Env::new()).unwrap());
    }

    // Existing saved comprehensions use string targets, which must retain their shape.
    let saved = serde_json::json!({"ListComprehension": {
        "value": {"Variable": "a"},
        "comprehension": {
            "targets": ["a", "b"],
            "iterable": {"Array": [{"Array": [{"Literal": {"Int": 1}}, {"Literal": {"Int": 2}}]}]},
            "filter": null
        }
    }});
    let restored = Expr::try_from(serde_json::from_value::<ExprSerializer>(saved.clone()).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(ExprSerializer::from(&restored)).unwrap(), saved);
    assert_eq!(engine().evaluate(&restored.to_string(), &Env::new()).unwrap(),
        EvalValue::Array(vec![EvalValue::Int(1)].into()));
}

#[test]
fn lexical_errors_have_kinds_and_character_positions() {
    use crate::err::LexError;

    for (source, position, kind) in [
        ("é + @", 4, LexError::UnexpectedCharacter('@')),
        ("?", 0, LexError::LoneQuestionMark),
        ("0x", 2, LexError::MissingDigits { radix: 16 }),
        ("0b102", 4, LexError::InvalidDigit { radix: 2 }),
        ("'abc", 0, LexError::UnterminatedString),
        ("'abc\\", 4, LexError::IncompleteEscape),
    ] {
        let error = Tokenizer::new(source).tokenize().unwrap_err();
        assert_eq!(error, Error::Lexical { position, kind }, "{source}");
    }

    assert!(matches!(
        parse_next("a + @").unwrap_err(),
        Error::Lexical {
            position: 4,
            kind: LexError::UnexpectedCharacter('@')
        }
    ));
    assert_eq!(parse_next("a, @").unwrap().1, 1);
}

#[test]
fn syntax_errors_survive_formula_context() {
    use crate::err::SyntaxError;

    let error = engine().evaluate("(1", &Env::new()).unwrap_err();
    assert_eq!(
        error.without_context(),
        &Error::Syntax {
            token_index: 2,
            kind: SyntaxError::ExpectedToken {
                expected: Token::RightParen,
                found: Token::End
            },
        }
    );
    let report = format!("{error:#}");
    assert!(report.contains("Syntax error"));
    assert!(report.contains("RightParen"));

    assert_eq!(
        parse_next("a +").unwrap_err(),
        Error::Syntax {
            token_index: 2,
            kind: SyntaxError::ExpectedExpression { found: Token::End },
        }
    );
    assert!(matches!(
        validate_formula_syntax("fn x, x -> x").unwrap_err().without_context(),
        Error::Syntax { kind: SyntaxError::DuplicateBinding { name }, .. } if name == "x"
    ));
    assert!(matches!(
        validate_formula_syntax("1, 2")
            .unwrap_err()
            .without_context(),
        Error::Syntax {
            kind: SyntaxError::TrailingToken {
                found: Token::Comma
            },
            ..
        }
    ));
}

#[test]
fn evaluation_errors_are_matchable_without_parsing_messages() {
    let mut engine = engine();
    let env = Env::new();
    assert!(matches!(
        engine
            .evaluate("int([])", &env)
            .unwrap_err()
            .without_context(),
        Error::Type(_)
    ));
    assert!(matches!(
        engine
            .evaluate("1 / 0", &env)
            .unwrap_err()
            .without_context(),
        Error::DivisionByZero(_)
    ));
    assert!(matches!(
        engine
            .evaluate("pow(2, 63)", &env)
            .unwrap_err()
            .without_context(),
        Error::Overflow(_)
    ));
    assert!(matches!(
        engine
            .evaluate("pow(-1, 0.5)", &env)
            .unwrap_err()
            .without_context(),
        Error::NumericDomain(_)
    ));
    assert!(matches!(
        engine
            .evaluate("range(0, 1, 0)", &env)
            .unwrap_err()
            .without_context(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        engine.evaluate("error('custom failure')", &env).unwrap_err().without_context(),
        Error::User(message) if message == "\"custom failure\""
    ));
    assert!(matches!(
        engine.evaluate("missing_name", &env).unwrap_err().without_context(),
        Error::UnknownVariable { name, .. } if name == "missing_name"
    ));
    assert!(matches!(
        engine
            .evaluate("{}.missing", &env)
            .unwrap_err()
            .without_context(),
        Error::UnknownMember { .. }
    ));
    assert!(matches!(
        engine.evaluate("{}.getpath('missing')", &env).unwrap_err().without_context(),
        Error::UnknownKey { key } if key == "missing"
    ));
    assert_eq!(
        engine
            .evaluate("[1][-2]", &env)
            .unwrap_err()
            .without_context(),
        &Error::IndexOutOfBounds {
            index: -2,
            length: 1
        }
    );
}

#[test]
fn arity_errors_report_the_accepted_and_actual_counts() {
    use crate::err::ArgumentCount;

    for (source, expected, actual) in [
        ("abs()", ArgumentCount::Exactly(1), 0),
        ("(fn x -> x)()", ArgumentCount::AtLeast(1), 0),
        (
            "round(1, 2, 3)",
            ArgumentCount::Between { min: 1, max: 2 },
            3,
        ),
        ("range()", ArgumentCount::Between { min: 1, max: 3 }, 0),
        ("[1].tryget()", ArgumentCount::Between { min: 2, max: 3 }, 1),
        (
            "[1].pop(0, 1)",
            ArgumentCount::Between { min: 1, max: 2 },
            3,
        ),
    ] {
        let error = engine().evaluate(source, &Env::new()).unwrap_err();
        let Error::Arity {
            expected: accepted,
            actual: supplied,
            ..
        } = error.without_context()
        else {
            panic!("{source}: {error:#}");
        };
        assert_eq!(*accepted, expected, "{source}");
        assert_eq!(*supplied, actual, "{source}");
    }
}

#[test]
fn numeric_conversion_errors_preserve_standard_sources() {
    use std::error::Error as _;
    use std::num::{IntErrorKind, ParseFloatError, ParseIntError};

    let error = EvalValue::String("invalid".into()).to_i64().unwrap_err();
    assert!(matches!(&error, Error::InvalidInteger { .. }));
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<ParseIntError>()
        .unwrap();
    assert_eq!(source.kind(), &IntErrorKind::InvalidDigit);

    let error = EvalValue::String("invalid".into()).to_f64().unwrap_err();
    assert!(matches!(&error, Error::InvalidFloat { .. }));
    assert!(error.source().unwrap().is::<ParseFloatError>());

    let error = parse_next("999999999999999999999999").unwrap_err();
    assert!(matches!(
        &error,
        Error::Lexical {
            kind: crate::err::LexError::InvalidInteger { .. },
            ..
        }
    ));
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<ParseIntError>()
        .unwrap();
    assert_eq!(source.kind(), &IntErrorKind::PosOverflow);
}

#[test]
fn builtin_deserialization_uses_typed_errors() {
    let error =
        Function::try_from(FunctionSerializer::Builtin("missing_builtin".into())).unwrap_err();
    assert!(matches!(&error, Error::InvalidBuiltin { name, .. } if name == "missing_builtin"));
    // Serde still reports the typed conversion failure through its native error.
    let error =
        serde_json::from_str::<EvalValue>(r#"{"Func":{"Builtin":"missing_builtin"}}"#).unwrap_err();
    assert!(error.to_string().contains("missing_builtin"));
}

#[test]
fn parse_next_stops_at_expression_boundary() {
    for (source, expected, end) in [
        ("a*a+3, b+5", "a*a+3", 5),
        ("a*a+3", "a*a+3", 5),
        ("  a + 3  , b", "a + 3", 7),
        ("a  \n", "a", 1),
        ("a b+5", "a", 1),
        ("a)", "a", 1),
        ("a; trailing text", "a", 1),
        ("a, 'unterminated", "a", 1),
        ("a @", "a", 1),
        ("a not something", "a", 1),
        ("a not @", "a", 1),
        ("é + '猫', rest", "é + '猫'", 10),
        ("'😀'  ", "'😀'", 6),
    ] {
        let (expression, position) = parse_next(source).unwrap();
        let expected = Parser::new(Tokenizer::new(expected).tokenize().unwrap())
            .parse()
            .unwrap();
        assert_eq!(expression, expected, "{source}");
        assert_eq!(position, end, "{source}");
    }
}

#[test]
fn parse_next_preserves_nested_expressions_and_continuations() {
    for source in [
        "f(a, [1, 2], {'key': 3}).key[0:2] + 4",
        "(a + b) * 3",
        "a if b not in [1, 2] else c ?? d",
        "let x = 1, y = 2 for x + y",
        "fn x, y -> x + y",
        "[x + 1 for x in [1, 2] if x > 0]",
        "{'key': x for x in [1, 2]}",
        "2d6",
        "2d6 + 1d4 * 2",
        "0xff + 0b10 + #ab",
        "'a, b' + 'c'",
    ] {
        let expected = Parser::new(Tokenizer::new(source).tokenize().unwrap())
            .parse()
            .unwrap();
        let (expression, end) = parse_next(&format!("{source}, @invalid suffix")).unwrap();
        assert_eq!(expression, expected, "{source}");
        assert_eq!(end, source.chars().count(), "{source}");
    }
}

#[test]
fn parse_next_rejects_invalid_expressions() {
    for source in [
        "", "   ", ", a", "@", "a +", "a +, b", "a + @", "a(",
        "a.", "a[1", "(a + b", "[1,] +", "{'a':}", "a if b",
        "a not in", "fn x ->", "let x = 1 for", "'unterminated",
        "0x", "0b102", "99999999999999999999999999999", "a + 'unterminated",
    ] {
        assert!(parse_next(source).is_err(), "{source:?}");
    }
}

#[test]
fn let_expressions_work_as_operator_operands() {
    let mut engine = engine();
    let env = Env::new();
    for (prefix, value, expected) in [
        ("1==", "1", EvalValue::Bool(true)),
        ("1!=", "2", EvalValue::Bool(true)),
        ("1<", "2", EvalValue::Bool(true)),
        ("1<=", "1", EvalValue::Bool(true)),
        ("2>", "1", EvalValue::Bool(true)),
        ("2>=", "2", EvalValue::Bool(true)),
        ("1 in ", "[1, 2]", EvalValue::Bool(true)),
        ("3 not in ", "[1, 2]", EvalValue::Bool(true)),
        ("1+", "2", EvalValue::Int(3)),
        ("4-", "2", EvalValue::Int(2)),
        ("3*", "2", EvalValue::Int(6)),
        ("4/", "2", EvalValue::Float(2.0)),
        ("5//", "2", EvalValue::Int(2)),
        ("5%", "2", EvalValue::Int(1)),
        ("1|", "2", EvalValue::Int(3)),
        ("3^", "2", EvalValue::Int(1)),
        ("3&", "2", EvalValue::Int(2)),
        ("1<<", "2", EvalValue::Int(4)),
        ("8>>", "2", EvalValue::Int(2)),
        ("8>>>", "2", EvalValue::Int(2)),
        ("True and ", "2", EvalValue::Int(2)),
        ("True&&", "2", EvalValue::Int(2)),
        ("False or ", "2", EvalValue::Int(2)),
        ("False||", "2", EvalValue::Int(2)),
        ("False xor ", "True", EvalValue::Bool(true)),
        ("null??", "2", EvalValue::Int(2)),
        ("not ", "False", EvalValue::Bool(true)),
        ("!", "False", EvalValue::Bool(true)),
        ("+", "2", EvalValue::Int(2)),
        ("-", "2", EvalValue::Int(-2)),
        ("~", "2", EvalValue::Int(-3)),
    ] {
        for spacing in ["", " "] {
            let source = format!("{prefix}{spacing}let x = {value} for x");
            assert_eq!(
                engine.evaluate(&source, &env).unwrap_or_else(|error| panic!("{source}: {error:#}")),
                expected,
                "{source}",
            );
        }
    }
}

#[test]
fn keyword_operands_preserve_scope_precedence_and_expression_boundaries() {
    for (source, grouped) in [
        ("1 < let x = 2 for x + 3", "1 < (let x = 2 for x + 3)"),
        ("2 * let x = 3 for x + 4", "2 * (let x = 3 for x + 4)"),
        ("-let x = 2 for x + 3", "-(let x = 2 for x + 3)"),
        ("0 < 1 < let x = 2 for x", "0 < 1 < (let x = 2 for x)"),
        ("1 < let x = 2 for x < 3", "1 < (let x = 2 for x < 3)"),
        ("null ?? fn x -> x + 1", "null ?? (fn x -> x + 1)"),
        ("False or fn -> 2", "False or (fn -> 2)"),
        ("True and fn -> 2", "True and (fn -> 2)"),
        ("null != fn -> 2", "null != (fn -> 2)"),
        ("!fn -> 2", "!(fn -> 2)"),
        ("1 if let x = True for x else 2", "1 if (let x = True for x) else 2"),
        ("[x for x in let xs = [1, 2] for xs]", "[x for x in (let xs = [1, 2] for xs)]"),
        ("[x for x in [1, 2] if let y = 1 for x > y]", "[x for x in [1, 2] if (let y = 1 for x > y)]"),
    ] {
        let expected = Parser::new(Tokenizer::new(grouped).tokenize().unwrap()).parse().unwrap();
        let actual = Parser::new(Tokenizer::new(source).tokenize().unwrap()).parse().unwrap();
        assert_eq!(actual, expected, "{source}");
        let (partial, end) = parse_next(&format!("{source}, @suffix")).unwrap();
        assert_eq!(partial, expected, "{source}");
        assert_eq!(end, source.chars().count(), "{source}");
        let rendered = actual.to_string();
        assert_eq!(Parser::new(Tokenizer::new(&rendered).tokenize().unwrap()).parse().unwrap(), actual, "{rendered}");
    }

    let mut engine = engine();
    let mut env = Env::new();
    env.insert("x", EvalValue::Int(10));
    assert_eq!(engine.evaluate("x + let x = 2, y = x + 1 for x * y", &env).unwrap(), EvalValue::Int(16));
    assert_eq!(env.get("x"), Some(&EvalValue::Int(10)));
    assert!(env.get("y").is_none());
    assert_eq!(engine.evaluate("(null ?? fn x -> x + 1)(2)", &env).unwrap(), EvalValue::Int(3));
    assert_eq!(engine.evaluate("True or let x = missing for x", &env).unwrap(), EvalValue::Bool(true));
    assert_eq!(engine.evaluate("{'let': 1, 'fn': 2}.let + {'fn': 2}.fn", &env).unwrap(), EvalValue::Int(3));
}

#[test]
fn incomplete_keyword_operands_are_syntax_errors() {
    use crate::err::SyntaxError;

    for source in ["1 < let", "1 + let x = 2 for", "null ?? fn", "True and fn x ->"] {
        let error = parse_next(source).unwrap_err();
        assert!(matches!(error, Error::Syntax {
            kind: SyntaxError::ExpectedBinding { .. } | SyntaxError::ExpectedExpression { .. },
            ..
        }), "{source}: {error}");
        assert!(validate_formula_syntax(source).is_err(), "{source}");
    }
}

    /// Creates the deterministic formula engine used by parser and evaluator tests.
    fn engine() -> FormulaEngine {
        FormulaEngine::new(Some(1))
    }

    // Verifies that arithmetic and floor division.
    #[test]
    fn arithmetic_and_floor_division() {
        let mut engine = engine();
        let env = Env::new();
        assert_eq!(
            engine.evaluate("2 + 3 * 4", &env).unwrap(),
            EvalValue::Int(14)
        );
        assert_eq!(
            engine.evaluate("-5 // 2", &env).unwrap(),
            EvalValue::Int(-3)
        );
    }

    // Verifies that python ternary and membership.
    #[test]
    fn python_ternary_and_membership() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("chance", EvalValue::Int(3));
        env.insert(
            "traits",
            EvalValue::Array(vec![EvalValue::String("danger".into())].into()),
        );
        assert_eq!(
            engine
                .evaluate("'or' if chance >= 2 else 'blanc'", &env)
                .unwrap(),
            EvalValue::String("or".into())
        );
        assert_eq!(
            engine.evaluate("'danger' in traits", &env).unwrap(),
            EvalValue::Bool(true)
        );
        assert_eq!(
            engine
                .evaluate("(fn value -> value in traits)('danger')", &env)
                .unwrap(),
            EvalValue::Bool(true)
        );
        assert_eq!(
            engine.evaluate("'safe' not in traits", &env).unwrap(),
            EvalValue::Bool(true)
        );

        let membership = Parser::new(
            Tokenizer::new("'danger' in traits")
                .tokenize()
                .unwrap(),
        )
        .parse()
        .unwrap();
        let non_membership = Parser::new(
            Tokenizer::new("'safe' not in traits")
                .tokenize()
                .unwrap(),
        )
        .parse()
        .unwrap();
        assert!(membership.to_string().contains(" in "));
        assert!(non_membership.to_string().contains(" not in "));
    }

    // Verifies exponentiation with integral and fractional results.
    #[test]
    fn pow_function() {
        let mut engine = engine();
        let env = Env::new();
        assert_eq!(
            engine.evaluate("pow(2, 10)", &env).unwrap(),
            EvalValue::Int(1024)
        );
        assert_eq!(
            engine.evaluate("pow(9, 0.5)", &env).unwrap(),
            EvalValue::Float(3.0)
        );
        assert_eq!(
            engine.evaluate("pow(2, -2)", &env).unwrap(),
            EvalValue::Float(0.25)
        );
        assert!(engine.evaluate("pow(-1, 0.5)", &env).is_err());
    }

    #[test]
    fn logical_aliases_preserve_values_and_short_circuiting() {
        let mut engine = engine();
        let env = Env::new();
        for (source, expected) in [
            ("False && (1 / 0)", EvalValue::Bool(false)),
            ("True || (1 / 0)", EvalValue::Bool(true)),
            ("0 && missing", EvalValue::Int(0)),
            ("7 || missing", EvalValue::Int(7)),
            ("2 && 3", EvalValue::Int(3)),
            ("0 || 3", EvalValue::Int(3)),
            ("'' || 'Anonymous'", EvalValue::String("Anonymous".into())),
            ("0 or 2 && 3", EvalValue::Int(3)),
            ("0 || 2 and 3", EvalValue::Int(3)),
            ("1 || 0 && missing", EvalValue::Int(1)),
            ("1 & 3 && 0 | 4", EvalValue::Int(4)),
            ("'&& || ! xor'", EvalValue::String("&& || ! xor".into())),
        ] {
            assert_eq!(engine.evaluate(source, &env).unwrap(), expected, "{source}");
        }
        for source in ["True && missing", "False || missing", "True &&& False", "True ||| False"] {
            assert!(engine.evaluate(source, &env).is_err(), "{source}");
        }
    }

    #[test]
    fn logical_negation_precedence_and_format_roundtrip() {
        let mut engine = engine();
        let env = Env::new();
        for (source, expected) in [
            ("!2 == 1", false),
            ("not 2 == 1", true),
            ("!(2 == 1)", true),
            ("!!2", true),
            ("!None", true),
            ("![]", true),
            ("![False][0]", true),
            ("2 != 1 && !False", true),
            ("not 2 in [2]", false),
            ("2 not in [1]", true),
        ] {
            assert_eq!(engine.evaluate(source, &env).unwrap(), EvalValue::Bool(expected), "{source}");
            let ast = Parser::new(Tokenizer::new(source).tokenize().unwrap()).parse().unwrap();
            assert_eq!(engine.evaluate(&ast.to_string(), &env).unwrap(), EvalValue::Bool(expected), "formatted {source}");
        }
    }

    #[test]
    fn logical_xor_truthiness_precedence_and_evaluation() {
        let mut engine = engine();
        let env = Env::new();
        for (source, expected) in [
            ("False xor False", false),
            ("False xor True", true),
            ("True xor False", true),
            ("True xor True", false),
            ("2 xor 3", false),
            ("[] xor 'present'", true),
            ("True xor True xor True", true),
            ("True or False xor True", true),
            ("True || False xor True", true),
            ("True xor True and False", true),
            ("True xor True && False", true),
            ("2 == 2 xor 3 == 4", true),
            ("not True xor True", true),
        ] {
            assert_eq!(engine.evaluate(source, &env).unwrap(), EvalValue::Bool(expected), "{source}");
            let ast = Parser::new(Tokenizer::new(source).tokenize().unwrap()).parse().unwrap();
            let serialized = serde_json::to_string(&ExprSerializer::from(&ast)).unwrap();
            let restored = Expr::try_from(serde_json::from_str::<ExprSerializer>(&serialized).unwrap()).unwrap();
            assert_eq!(engine.evaluate(&restored.to_string(), &env).unwrap(), EvalValue::Bool(expected), "roundtrip {source}");
        }
        for source in ["True xor missing", "False xor missing", "let xor = 1 for xor", "fn xor -> xor"] {
            assert!(engine.evaluate(source, &env).is_err(), "{source}");
        }
        // Different predicates expose operand order; the next roll exposes extra evaluations.
        for seed in 0..16 {
            let mut actual = FormulaEngine::new(Some(seed));
            let mut reference = FormulaEngine::new(Some(seed));
            let left = reference.evaluate("dice(1, 6) > 2", &env).unwrap().truthy();
            let right = reference.evaluate("dice(1, 6) > 4", &env).unwrap().truthy();
            assert_eq!(actual.evaluate("dice(1, 6) > 2 xor dice(1, 6) > 4", &env).unwrap(), EvalValue::Bool(left != right));
            assert_eq!(actual.evaluate("dice(1, 6)", &env).unwrap(), reference.evaluate("dice(1, 6)", &env).unwrap());
        }
    }

    #[test]
    fn hash_hex_literals_match_hex_prefix() {
        let mut engine = engine();
        let env = Env::new();
        for digits in [
            "0",
            "f",
            "FFF",
            "12aBcD",
            "0000FF",
            "2d6",
            "7FFFFFFFFFFFFFFF",
        ] {
            let hash = format!("#{digits}");
            let hex = format!("0x{digits}");
            assert_eq!(
                Tokenizer::new(&hash).tokenize().unwrap(),
                Tokenizer::new(&hex).tokenize().unwrap()
            );
            assert_eq!(
                engine.evaluate(&hash, &env).unwrap(),
                engine.evaluate(&hex, &env).unwrap()
            );
        }
        assert_eq!(
            engine.evaluate("(#123456 >> 8) & #FF", &env).unwrap(),
            EvalValue::Int(0x34)
        );
        assert_eq!(
            engine.evaluate("-#FF + #100", &env).unwrap(),
            EvalValue::Int(1)
        );
        assert_eq!(
            engine.evaluate("${#FFFFFF}", &env).unwrap(),
            EvalValue::Int(0xFFFFFF)
        );
        assert_eq!(
            engine.evaluate("'#123456'", &env).unwrap(),
            EvalValue::String("#123456".into())
        );
        assert_eq!(
            engine.format("value=${#FF}".into(), &env).unwrap().as_ref(),
            "value=255"
        );
    }

    #[test]
    fn hash_hex_literals_reject_invalid_digits_and_overflow() {
        let mut engine = engine();
        for digits in [
            "",
            "G",
            "12G4",
            "FF_FF",
            "8000000000000000",
            "FFFFFFFFFFFFFFFF",
            "0x10",
        ] {
            for prefix in ["#", "0x"] {
                let source = format!("{prefix}{digits}");
                assert!(engine.evaluate(&source, &Env::new()).is_err(), "{source}");
            }
        }
        assert!(engine.evaluate("# FF", &Env::new()).is_err());
    }

    // Verifies bitwise operators, radix literals, and their precedence.
    #[test]
    fn bitwise_operators_and_radix_literals() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("flags", EvalValue::Int(5));

        assert_eq!(
            engine.evaluate("0xF0 & 0b10101100", &env).unwrap(),
            EvalValue::Int(0xA0)
        );
        assert_eq!(
            engine.evaluate("0o17 ^ 0b0011", &env).unwrap(),
            EvalValue::Int(12)
        );
        assert_eq!(
            engine.evaluate("3 | 4 & 1", &env).unwrap(),
            EvalValue::Int(3)
        );
        assert_eq!(
            engine.evaluate("flags & 0x04 != 0", &env).unwrap(),
            EvalValue::Bool(true)
        );
        assert!(engine.evaluate("1.0 & 1", &env).is_err());
        assert!(engine.evaluate("0b102", &env).is_err());
    }

    /// Verifies named parameters, args, caller environment access, and function composition.
    #[test]
    fn user_defined_functions() {
        let mut functions: IndexMap<String, FormulaFunction> = IndexMap::new();
        let func = Parser::new(Tokenizer::new("'ready' if status == 'pending' else 'done' if status == 'ready' else status").tokenize().unwrap()).parse().unwrap();
        functions.insert(
            "advance_status".into(),
            FormulaFunction {
                parameters: Some(vec!["status".into()].into()),
                body: func.into(),
            },
        );
        let func = Parser::new(Tokenizer::new("args.0 + args[1]").tokenize().unwrap()).parse().unwrap();
        functions.insert(
            "sum_args".into(),
            FormulaFunction {
                parameters: None,
                body: func.into(),
            },
        );
        let func = Parser::new(Tokenizer::new("prefix + advance_status(status)").tokenize().unwrap()).parse().unwrap();
        functions.insert(
            "status_label".into(),
            FormulaFunction {
                parameters: Some(vec!["status".into()].into()),
                body: func.into(),
            },
        );
        let functions = {
            let mut env = Env::new();
            for (name, value) in functions {
                env.insert(name, EvalValue::Func(
                    Function::User(
                        FormulaClosure::without_captures(value)
                    ).into()
                ));
            }
            env.with_parent(&GLOBAL_ENV)
        };
        let mut engine = FormulaEngine::new(Some(1)).with_globals(functions);
        let mut env = Env::new();
        env.insert("prefix", EvalValue::String("Status: ".into()));

        assert_eq!(
            engine.evaluate("advance_status('ready')", &env).unwrap(),
            EvalValue::String("done".into())
        );
        assert_eq!(
            engine.evaluate("sum_args(4, 7)", &env).unwrap(),
            EvalValue::Int(11)
        );
        assert_eq!(
            engine.evaluate("status_label('pending')", &env).unwrap(),
            EvalValue::String("Status: ready".into())
        );
        assert!(engine.evaluate("advance_status()", &env).is_err());
    }

    /// Verifies that recursive user functions fail with a bounded call-chain error.
    #[test]
    fn user_function_recursion_is_bounded() {
        let mut functions: IndexMap<String, FormulaFunction> = IndexMap::new();
        let func = Parser::new(Tokenizer::new("loop()").tokenize().unwrap()).parse().unwrap();
        functions.insert(
            "loop".into(),
            FormulaFunction {
                parameters: None,
                body: func.into(),
            },
        );
        let functions = {
            let mut env = Env::new();
            for (name, value) in functions {
                env.insert(name, EvalValue::Func(
                    Function::User(
                        FormulaClosure::without_captures(value)
                    ).into()
                ));
            }
            env.with_parent(&GLOBAL_ENV)
        };
        let mut engine = FormulaEngine::new(Some(1)).with_globals(functions);
        let error = engine.evaluate("loop()", &Env::new()).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("function `loop`") || error.contains("Maximum function call depth"));
    }

    #[test]
    fn builtin_and_mixed_recursion_share_the_call_limit() {
        fn reenter(_: &[EvalValue], engine: &mut FormulaEngine, env: &Env) -> Result<EvalValue> {
            engine.evaluate("reenter()", env)
        }

        let mut env = Env::new();
        env.insert("source", "eval(source)".into());
        env.insert("formatted", "${eval(formatted)}".into());
        let (name, function) = EvalValue::func("reenter", reenter);
        env.insert(name, function);
        let mut engine = engine().with_max_rec(8);
        for (source, frame) in [
            ("eval(source)", "eval"),
            ("eval(formatted)", "eval"),
            ("let f = fn -> eval('f()') for f()", "fn()"),
            ("reenter()", "reenter"),
        ] {
            let error = engine.evaluate(source, &env).unwrap_err();
            let Error::RecursionLimit { limit, call_chain } = error.without_context() else {
                panic!("expected recursion limit for {source}: {error:#}");
            };
            assert_eq!(*limit, 8);
            assert_eq!(call_chain.split(" -> ").count(), 9);
            assert!(call_chain.contains(frame), "{call_chain}");
            assert_eq!(engine.evaluate("abs(-7)", &env).unwrap(), EvalValue::Int(7));
        }
    }

    #[test]
    fn try_catches_recursive_eval_and_releases_call_frames() {
        let mut env = Env::new();
        env.insert("source", "eval(source)".into());
        let mut engine = engine().with_max_rec(4);
        for _ in 0..2 {
            assert_eq!(
                engine.evaluate("try(fn -> eval(source), fn e -> e.kind)", &env).unwrap(),
                EvalValue::from("RecursionLimit")
            );
            assert_eq!(engine.evaluate("try(fn -> eval(source))", &env).unwrap(), EvalValue::Null);
            assert_eq!(engine.evaluate("(fn -> abs(-7))()", &env).unwrap(), EvalValue::Int(7));
        }
    }

    #[test]
    fn call_limit_counts_each_active_function_once_and_recovers_from_errors() {
        let env = Env::new();
        let mut engine = engine().with_max_rec(1);
        assert_eq!(engine.evaluate("(fn -> 7)()", &env).unwrap(), EvalValue::Int(7));
        assert_eq!(engine.evaluate("abs(-7)", &env).unwrap(), EvalValue::Int(7));
        let error = engine.evaluate("(fn -> abs(-7))()", &env).unwrap_err();
        assert!(matches!(error.without_context(), Error::RecursionLimit { limit: 1, .. }));
        for source in ["abs()", "(fn x -> x)()", "(fn [x] -> x)([])", "(fn -> missing)()"] {
            assert!(engine.evaluate(source, &env).is_err(), "{source}");
            assert_eq!(engine.evaluate("(fn -> 7)()", &env).unwrap(), EvalValue::Int(7));
        }

        let mut engine = engine.with_max_rec(0);
        assert_eq!(engine.evaluate("7", &env).unwrap(), EvalValue::Int(7));
        for source in ["abs(-7)", "(fn -> 7)()"] {
            let error = engine.evaluate(source, &env).unwrap_err();
            assert!(matches!(error.without_context(), Error::RecursionLimit { limit: 0, .. }));
        }
    }

    #[test]
    fn sequential_callbacks_do_not_accumulate_call_depth() {
        let mut engine = engine().with_max_rec(2);
        assert_eq!(
            engine.evaluate("range(100).map(fn value -> value + 1).sum()", &Env::new()).unwrap(),
            EvalValue::Int(5050)
        );
    }

    /// Verifies anonymous function parsing, ordinary value dispatch, and extra arguments.
    #[test]
    fn anonymous_functions_are_first_class_values() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("offset", EvalValue::Int(3));
        assert_eq!(
            engine
                .evaluate("(fn value -> value + offset)(4, 99)", &env)
                .unwrap(),
            EvalValue::Int(7)
        );
        assert_eq!(
            engine
                .evaluate(
                    "let add = fn left, right -> left + right for add(2, 5)",
                    &env
                )
                .unwrap(),
            EvalValue::Int(7)
        );
        assert!(engine
            .evaluate("(fn left, right -> left + right)(2)", &env)
            .is_err());
    }

    /// A type error identifies the offending value, the contract it violated, and every
    /// formula/function layer crossed while it bubbled up.
    #[test]
    fn nested_call_errors_keep_value_reason_and_context_chain() {
        let mut engine = engine();
        let error = engine
            .evaluate("(fn value -> int(value))([1, 2])", &Env::new())
            .unwrap_err();
        let chain = error
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>();
        let report = chain.join("\n");

        assert!(report.contains("[1, 2] (type list)"));
        assert!(report.contains("only int, float, bool"));
        assert!(report.contains("Conversion to `int`"));
        assert!(report.contains("In fn(value)"));
        assert!(report.contains("Call `int`"));
        assert!(report.contains("Error in formula"));
        assert!(chain.len() >= 6, "error chain too short: {report}");
    }

    /// Unknown-name diagnostics explain the lookup failure and expose useful visible names.
    #[test]
    fn unknown_name_errors_list_visible_bindings() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("settings", EvalValue::Null);
        let error = engine.evaluate("setings", &env).unwrap_err();
        let report = format!("{error:#}");

        assert!(report.contains("Unknown name `setings`"));
        assert!(report.contains("no variable with this name"));
    }

    /// Verifies that returned functions retain outer parameters after their caller returns.
    #[test]
    fn nested_functions_capture_outer_bindings() {
        let mut engine = engine();
        let env = Env::new();

        assert_eq!(
            engine
                .evaluate("(fn x -> fn y -> x + y)(2)(3)", &env)
                .unwrap(),
            EvalValue::Int(5)
        );
        assert_eq!(
            engine
                .evaluate(
                    "let offset = 4 for (fn -> fn value -> value + offset)()(3)",
                    &env,
                )
                .unwrap(),
            EvalValue::Int(7)
        );
        assert_eq!(
            engine
                .evaluate(
                    "let funcs = [fn -> value for value in [2, 5]] for funcs[0]() + funcs[1]()",
                    &env,
                )
                .unwrap(),
            EvalValue::Int(7)
        );
    }

    /// Referenced local values are snapshots, independent of the calling environment.
    #[test]
    fn closures_capture_only_referenced_lexical_values() {
        let mut engine = engine();
        let mut creation_env = Env::new();
        creation_env.insert("used", EvalValue::Int(4));
        creation_env.insert("unused", EvalValue::Int(99));
        creation_env.insert("label", EvalValue::String("original".into()));
        creation_env.insert("settings", engine.evaluate("{'enabled': true}", &Env::new()).unwrap());
        let closure = engine
            .evaluate("fn value -> [value, used, label, settings]", &creation_env)
            .unwrap();
        let EvalValue::Func(function) = &closure else { panic!("expected a function") };
        let Function::User(function) = function.as_ref() else { panic!("expected a user function") };
        assert_eq!(
            function.captures.keys().map(|name| name.as_ref()).collect::<Vec<_>>(),
            vec!["used", "label", "settings"]
        );

        let mut call_env = Env::new();
        call_env.insert("used", EvalValue::Int(100));
        call_env.insert("label", EvalValue::String("replacement".into()));
        call_env.insert("settings", EvalValue::Null);
        let expected = engine.evaluate("[7, 4, 'original', {'enabled': true}]", &Env::new()).unwrap();
        for env in [&call_env, &Env::new()] {
            assert_eq!(closure.clone().call(&[EvalValue::Int(7)], &mut engine, env).unwrap(), expected);
        }
    }

    #[test]
    fn closures_keep_snapshots_and_resolve_unbound_names() {
        let mut engine = engine();
        let mut creation_env = Env::new();
        creation_env.insert("current", EvalValue::Int(3));
        let snapshot = engine.evaluate("let saved = current for fn -> saved", &creation_env).unwrap();
        let dynamic = engine.evaluate("fn -> current", &Env::new()).unwrap();
        let mut call_env = Env::new();
        call_env.insert("current", EvalValue::Int(8));
        call_env.insert("saved", EvalValue::Int(99));
        assert_eq!(snapshot.call(&[], &mut engine, &call_env).unwrap(), EvalValue::Int(3));
        assert_eq!(dynamic.clone().call(&[], &mut engine, &call_env).unwrap(), EvalValue::Int(8));
        assert!(matches!(
            dynamic.call(&[], &mut engine, &Env::new()).unwrap_err().without_context(),
            Error::UnknownVariable { name, .. } if name == "current"
        ));
    }

    #[test]
    fn serialized_closures_preserve_definition_and_captures() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("offset", EvalValue::Int(5));
        let EvalValue::Func(function) = engine.evaluate("fn value -> value + offset", &env).unwrap()
            else { panic!("expected a function") };
        let json = serde_json::to_value(FunctionSerializer::from(function.as_ref())).unwrap();
        let fields = json["User"].as_object().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields.contains_key("definition"));
        assert!(fields.contains_key("captures"));
        let restored = Function::try_from(serde_json::from_value::<FunctionSerializer>(json).unwrap()).unwrap();
        assert_eq!(
            EvalValue::Func(Arc::new(restored)).call(&[EvalValue::Int(7)], &mut engine, &Env::new()).unwrap(),
            EvalValue::Int(12)
        );
    }

    #[test]
    fn binding_origins_survive_environment_merges() {
        let mut local_values = IndexMap::new();
        local_values.insert("x".into(), EvalValue::Int(1));
        local_values.insert("local_only".into(), EvalValue::Int(2));
        let local = Env::from_index_map(&local_values);
        let mut global = Env::new();
        global.insert("x", EvalValue::Int(1));
        global.insert("engine_only", EvalValue::Int(3));
        let global = global.into_engine_globals();

        for merged in [
            Env::new().with_parent(&global).with_parent(&local),
            Env::merge(&[&local, &global]),
            local.merged(&global),
            local.clone().expand(global.clone()),
        ] {
            for env in [merged.clone(), merged.solidified()] {
                assert_eq!(env.get_binding("x").unwrap().1, BindingOrigin::EngineGlobal);
                assert_eq!(
                    env.get_binding("engine_only").unwrap().1,
                    BindingOrigin::EngineGlobal
                );
                assert_eq!(
                    env.get_binding("local_only").unwrap().1,
                    BindingOrigin::Local
                );
            }
        }
        let reversed = Env::new()
            .with_parent(&local)
            .with_parent(&global)
            .solidified();
        assert_eq!(
            reversed.get_binding("x").unwrap().1,
            BindingOrigin::Local
        );

        // All visible names become engine globals when this environment is installed.
        let installed = Env::new()
            .with_parent(&local)
            .with_parent(&global)
            .into_engine_globals();
        assert_eq!(
            installed.get_binding("local_only").unwrap().1,
            BindingOrigin::EngineGlobal
        );
        assert_eq!(
            installed.get_binding("engine_only").unwrap().1,
            BindingOrigin::EngineGlobal
        );
    }

    #[test]
    fn assigning_equal_values_creates_local_bindings() {
        let values = IndexMap::from([("x".into(), EvalValue::Int(1))]);
        let global = Env::from_index_map(&values).into_engine_globals();
        for operation in 0..4 {
            let mut local = global.clone();
            match operation {
                0 => local.insert("x", EvalValue::Int(1)),
                1 => local.extend(&values),
                2 => local = local.expand(Env::from_index_map(&values)),
                3 => local = local.expand(Env::from_arc(Arc::new(values.clone()))),
                _ => unreachable!(),
            }
            assert_eq!(local.get_binding("x").unwrap().1, BindingOrigin::Local);
            assert_eq!(
                global.get_binding("x").unwrap().1,
                BindingOrigin::EngineGlobal
            );
        }
        let local = Env::from_index_map(&values);
        assert_eq!(
            global
                .merged(&local)
                .solidified()
                .get_binding("x")
                .unwrap()
                .1,
            BindingOrigin::Local
        );
    }

    /// Verifies that globals remain global unless a lexical binding shadows their name.
    #[test]
    fn closures_distinguish_globals_from_lexical_shadows() {
        let mut engine = engine();
        let env = Env::new();
        let global_sum = engine.evaluate("fn -> sum([1, 2, 3])", &env).unwrap();

        let shadow = engine.evaluate("fn values -> 99", &env).unwrap();
        let mut shadow_env = Env::new();
        shadow_env.insert("sum", shadow);
        let captured_sum = engine
            .evaluate("fn -> sum([1, 2, 3])", &shadow_env)
            .unwrap();

        assert_eq!(
            global_sum.call(&[], &mut engine, &shadow_env).unwrap(),
            EvalValue::Int(6)
        );
        assert_eq!(
            captured_sum.call(&[], &mut engine, &env).unwrap(),
            EvalValue::Int(99)
        );

        let mut initial_globals = Env::new();
        initial_globals.insert("scale", EvalValue::Int(2));
        engine = engine.with_globals(initial_globals.with_parent(&GLOBAL_ENV));
        let global_scale = engine.evaluate("fn -> scale", &Env::new()).unwrap();

        // Even an equal-valued local binding is a lexical shadow.
        let mut local_env = Env::new();
        local_env.insert("scale", EvalValue::Int(2));
        let captured_scale = engine.evaluate("fn -> scale", &local_env).unwrap();

        let mut current_globals = Env::new();
        current_globals.insert("scale", EvalValue::Int(3));
        engine = engine.with_globals(current_globals.with_parent(&GLOBAL_ENV));
        assert_eq!(global_scale.call(&[], &mut engine, &local_env).unwrap(), EvalValue::Int(3));
        assert_eq!(captured_scale.call(&[], &mut engine, &Env::new()).unwrap(), EvalValue::Int(2));
    }

    /// Verifies receiver insertion, higher-order list helpers, and stored-member precedence.
    #[test]
    fn registered_methods_and_higher_order_helpers() {
        let mut engine = engine();
        let env = Env::new();

        assert_eq!(
            engine
                .evaluate("[1, 2, 3].map(fn value -> value * 2)", &env)
                .unwrap(),
            EvalValue::Array(vec![EvalValue::Int(2), EvalValue::Int(4), EvalValue::Int(6)].into())
        );
        assert_eq!(
            engine
                .evaluate("[1, 2, 3, 4].filter(fn value -> value % 2 == 0)", &env)
                .unwrap(),
            EvalValue::Array(vec![EvalValue::Int(2), EvalValue::Int(4)].into())
        );
        assert_eq!(
            engine
                .evaluate("[1, 2, 3].cumul(fn total, value -> total + value)", &env)
                .unwrap(),
            EvalValue::Array(vec![EvalValue::Int(1), EvalValue::Int(3), EvalValue::Int(6)].into())
        );
        assert_eq!(
            engine
                .evaluate("[2, 3].apply(fn left, right -> left * right)", &env)
                .unwrap(),
            EvalValue::Int(6)
        );
        assert_eq!(
            engine
                .evaluate("{'map': fn value -> value + 1}.map(4)", &env)
                .unwrap(),
            EvalValue::Int(5)
        );
    }

    /// Verifies that built-ins are supplied as a parent without explicit environment setup.
    #[test]
    fn builtins_are_ordinary_global_values() {
        let mut engine = engine();
        let env = Env::new();
        assert_eq!(
            engine.evaluate("sum([1, 2, 3])", &env).unwrap(),
            EvalValue::Int(6)
        );
        assert_eq!(
            engine
                .evaluate("apply(fn left, right -> left - right, [7, 2])", &env)
                .unwrap(),
            EvalValue::Int(5)
        );
    }

    // Verifies detection of dynamic text substitutions.
    #[test]
    fn formula_substitution_detection() {
        assert!(has_formula_substitution("$destination"));
        assert!(has_formula_substitution("section_${number}"));
        assert!(!has_formula_substitution("plain_text"));
        assert!(!has_formula_substitution("$$destination"));
    }

    // Verifies that formatting and escaped dollar.
    #[test]
    fn formatting_and_escaped_dollar() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("name", EvalValue::String("Violet".into()));
        env.insert("count", EvalValue::Int(7));
        assert_eq!(
            engine
                .format("$name has ${count + 1} items and $$3.".into(), &env)
                .unwrap(),
            "\"Violet\" has 8 items and $3.".into()
        );
    }

    // Verifies list literals, concatenation, comprehensions, filters, and destructuring.
    #[test]
    fn list_expressions_and_comprehensions() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert(
            "nums",
            EvalValue::Array(vec![EvalValue::Int(1), EvalValue::Int(2), EvalValue::Int(3)].into()),
        );
        env.insert(
            "pairs",
            EvalValue::Array(
                vec![
                    EvalValue::Array(vec![EvalValue::Int(1), EvalValue::Int(4)].into()),
                    EvalValue::Array(vec![EvalValue::Int(2), EvalValue::Int(5)].into()),
                ]
                .into(),
            ),
        );
        assert_eq!(
            engine
                .evaluate("[0] + [i + 1 for i in nums if i > 1]", &env)
                .unwrap(),
            EvalValue::Array(vec![EvalValue::Int(0), EvalValue::Int(3), EvalValue::Int(4)].into())
        );
        assert_eq!(
            engine
                .evaluate("[key + val for key, val in pairs]", &env)
                .unwrap(),
            EvalValue::Array(vec![EvalValue::Int(5), EvalValue::Int(7)].into())
        );
    }

    // Verifies Python-like list reducers, including their empty-list identities.
    #[test]
    fn list_reducer_methods() {
        let mut engine = engine();
        let env = Env::new();
        assert_eq!(
            engine.evaluate("[1, 2, 3].sum()", &env).unwrap(),
            EvalValue::Int(6)
        );
        assert_eq!(
            engine.evaluate("[1, 2.5, True].sum()", &env).unwrap(),
            EvalValue::Float(4.5)
        );
        assert_eq!(
            engine.evaluate("[].sum()", &env).unwrap(),
            EvalValue::Int(0)
        );
        assert_eq!(
            engine.evaluate("[1, 'oui', True].all()", &env).unwrap(),
            EvalValue::Bool(true)
        );
        assert_eq!(
            engine.evaluate("[0, '', 4].any()", &env).unwrap(),
            EvalValue::Bool(true)
        );
        assert_eq!(
            engine.evaluate("[].all()", &env).unwrap(),
            EvalValue::Bool(true)
        );
        assert_eq!(
            engine.evaluate("[].any()", &env).unwrap(),
            EvalValue::Bool(false)
        );
    }

    // Verifies scoped, sequential single and multiple `let` bindings.
    #[test]
    fn let_expressions_are_scoped_and_sequential() {
        let mut engine = engine();
        let mut env = Env::new();
        env.insert("x", EvalValue::Int(20));
        assert_eq!(
            engine
                .evaluate("let x = 2, y = x + 3 for x * y", &env)
                .unwrap(),
            EvalValue::Int(10)
        );
        assert_eq!(
            engine
                .evaluate("let x = 2 for let y = x + 1 for [x, y]", &env)
                .unwrap(),
            EvalValue::Array(vec![EvalValue::Int(2), EvalValue::Int(3)].into())
        );
        assert_eq!(env.get("x"), Some(&EvalValue::Int(20)));
        assert!(env.get("y").is_none());
    }

    // Verifies dictionary literals, right-biased merging, and key/value comprehensions.
    #[test]
    fn dictionary_expressions_and_comprehensions() {
        let mut engine = engine();
        let mut numbers = IndexMap::new();
        numbers.insert("one".into(), EvalValue::Int(1));
        numbers.insert("two".into(), EvalValue::Int(2));
        let mut env = Env::new();
        env.insert("nums", EvalValue::Object(numbers.into()));

        let mut expected_merge = IndexMap::new();
        expected_merge.insert("a".into(), EvalValue::Int(3));
        expected_merge.insert("b".into(), EvalValue::Int(2));
        assert_eq!(
            engine
                .evaluate("{'a': 1} + {'b': 2, 'a': 3}", &env)
                .unwrap(),
            EvalValue::Object(expected_merge.into())
        );

        assert_eq!(
            engine
                .evaluate("{key: val + 1 for key, val in nums if val > 1}", &env)
                .unwrap(),
            EvalValue::Object(IndexMap::from([("two".into(), EvalValue::Int(3),)]).into())
        );
    }

    // Verifies that dictionary braces and quoted braces do not terminate inline formulas early.
    #[test]
    fn dictionary_formulas_are_delimited_correctly() {
        let mut engine = engine();
        assert_eq!(
            engine
                .format(
                    "Result: ${{'brace': '}'}.brace}.".into(),
                    &Env::new()
                )
                .unwrap(),
            "Result: \"}\".".into()
        );
        assert_eq!(
            full_formula("${{'a': 1} + {'b': 2}}"),
            Some("{'a': 1} + {'b': 2}".into())
        );
        assert_eq!(full_formula("${1} ${2}"), None);

        assert_eq!(
            engine
                .evaluate(
                    r#"{'formule': '${id == 'record' and target == $target}'}"#,
                    &Env::new(),
                )
                .unwrap(),
            EvalValue::Object(
                IndexMap::from([(
                    "formule".into(),
                    EvalValue::String("${id == 'record' and target == $target}".into()),
                )])
                .into()
            )
        );
        assert_eq!(
            engine
                .evaluate(r##"{"formule": "${id == "record"}"}"##, &Env::new(),)
                .unwrap(),
            EvalValue::Object(
                IndexMap::from([(
                    "formule".into(),
                    EvalValue::String("${id == \"record\"}".into()),
                )])
                .into()
            )
        );
    }
