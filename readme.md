# Formulas

A small crate to parse and evaluate expression.

While not a true programming languages, it allows some pretty funky stuffs.

The full syntax is in the `syntax.pdf` document.

## Usage

Create a mutable `FormulaEngine` with `FormulaEngine::new(None)`, or pass `Some(seed)` for reproducible random operations. The engine includes the default builtin functions. Supply your own variables through an `Env`, inserting each value as an `EvalValue`.

To parse a formula without running it, tokenize the source with `Tokenizer::new(source).tokenize()`, then call `Parser::new(tokens).parse()`. This returns an `Expr` syntax tree and checks the complete expression's syntax; variable lookup and type checking happen during evaluation. If you only need validation, use `validate_formula_syntax(source)`.

Call `engine.evaluate(source, &env)` to evaluate a formula and receive an `EvalValue`. This method handles parsing itself and caches the parsed expression, so separate parsing is optional. Reuse the engine to evaluate the same formula with updated variables; results are recalculated on every call.

`engine.evalexpr(expr, &env)` can be used to evaluate an already parsed expression with a given environment.

```rust
use formulas::{Env, EvalValue, FormulaEngine, Parser, Tokenizer};

fn main() -> formulas::Result<()> {
    let mut engine = FormulaEngine::new(Some(42));
    let mut env = Env::new();
    env.insert("price", EvalValue::Int(12));
    env.insert("quantity", EvalValue::Int(3));

    let source = "price * quantity";

    // Optional: inspect the syntax tree without evaluating it.
    let tokens = Tokenizer::new(source).tokenize()?;
    let expression = Parser::new(tokens).parse()?;
    println!("Parsed: {expression}");

    let result = engine.evaluate(source, &env)?;
    assert_eq!(result, EvalValue::Int(36));

    env.insert("quantity", EvalValue::Int(4));
    assert_eq!(engine.evaluate(source, &env)?, EvalValue::Int(48));
    Ok(())
}
```

Parsing and evaluation return `formulas::Result`, so errors can be propagated with `?`. To handle a specific error, match on `error.without_context()`; format it with `format!("{error:#}")` to include the diagnostic context.

## Custom Builtin Functions

To make your custom builtin functions, first create an `Env`:

```rust
let mut env = Env::new();
```

Then wrap your custom function in an `EvalValue` and add it to the env. Its signature must be `fn (&[EvalValue], &mut FormulaEngine, &Env) -> Result<EvalValue>`:

```rust
let (key, f) = EvalValue::func("name", func);
env.insert(key, f);
```

Finally, pass the new global env to the engine. You can set the default global env as a parent if you still want to keep those functions:

```rust
let formula = FormulaEngine::new(None).with_globals(env.with_parent(&GLOBAL_ENV))
```
