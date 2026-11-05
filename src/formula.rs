//! Tokenizer, parser, runtime values, and evaluator for the formula language.
//!
//! The recursive-descent parser implements explicit precedence levels. Evaluation is
//! dynamically typed through [`EvalValue`] and uses the deterministic RNG owned by
//! [`FormulaEngine`]. Strings returned by formulas are not recursively evaluated unless
//! they are passed through a later explicit resolution step.
use crate::builtins::GLOBAL_ENV;
use crate::err::{bail, error, ArgumentCount, Error, LexError, Result, ResultContext, SyntaxError};
use indexmap::{IndexMap, IndexSet};
use rand::{Rng, SeedableRng, RngExt};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use serde::de::{Error as DeError};
use std::sync::LazyLock as Lazy;

pub static INT: Lazy<Arc<str>> = Lazy::new(|| Arc::from("int"));
pub static FLOAT: Lazy<Arc<str>> = Lazy::new(|| Arc::from("float"));
pub static BOOL: Lazy<Arc<str>> = Lazy::new(|| Arc::from("bool"));
pub static STR: Lazy<Arc<str>> = Lazy::new(|| Arc::from("str"));
pub static LIST: Lazy<Arc<str>> = Lazy::new(|| Arc::from("list"));
pub static DICT: Lazy<Arc<str>> = Lazy::new(|| Arc::from("dict"));
pub static NULL: Lazy<Arc<str>> = Lazy::new(|| Arc::from("null"));
pub static FUNCTION: Lazy<Arc<str>> = Lazy::new(|| Arc::from("func"));

/// Compares an integer and a float, taking into account the exact bounds of i64 and the precision of f64.
fn cmp_int_float(i: i64, f: f64) -> Ordering {
    // Every i64 becomes a finite float, so rounding cannot
    // affect its position relative to infinities or NaNs.
    if !f.is_finite() {
        return (i as f64).total_cmp(&f);
    }

    // Exact bounds: -2^63 inclusive, +2^63 exclusive.
    const UPPER: f64 = 9_223_372_036_854_775_808.0;
    const LOWER: f64 = -UPPER;

    if f < LOWER {
        return Ordering::Greater;
    }
    if f >= UPPER {
        return Ordering::Less;
    }

    let whole = f as i64; // Truncates toward zero.

    match i.cmp(&whole) {
        Ordering::Equal => (i as f64).total_cmp(&f),
        order => order,
    }
}

/// Represents a value understood by the formula language at runtime.
#[derive(Clone, Debug)]
pub enum EvalValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Arc<str>),
    Array(Arc<[EvalValue]>),
    Object(Arc<IndexMap<Arc<str>, EvalValue>>),
    Func(Arc<Function>),
}
impl PartialEq for EvalValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (EvalValue::Null, EvalValue::Null) => true,
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a == b,
            (EvalValue::Int(a), EvalValue::Int(b)) => a == b,
            (EvalValue::Float(a), EvalValue::Float(b)) => a == b,
            (EvalValue::Int(a), EvalValue::Float(b)) => cmp_int_float(*a, *b) == Ordering::Equal,
            (EvalValue::Int(a), EvalValue::Bool(true)) => *a == 1,
            (EvalValue::Int(a), EvalValue::Bool(false)) => *a == 0,
            (EvalValue::Bool(true), EvalValue::Int(b)) => *b == 1,
            (EvalValue::Bool(false), EvalValue::Int(b)) => *b == 0,
            (EvalValue::Float(a), EvalValue::Bool(true)) => *a == 1.0,
            (EvalValue::Float(a), EvalValue::Bool(false)) => *a == 0.0,
            (EvalValue::Bool(true), EvalValue::Float(b)) => *b == 1.0,
            (EvalValue::Bool(false), EvalValue::Float(b)) => *b == 0.0,
            (EvalValue::Float(a), EvalValue::Int(b)) => cmp_int_float(*b, *a) == Ordering::Equal,
            (EvalValue::String(a), EvalValue::String(b)) => a == b,
            (EvalValue::Array(a), EvalValue::Array(b)) => a == b,
            (EvalValue::Object(a), EvalValue::Object(b)) => a == b,
            (EvalValue::Func(a), EvalValue::Func(b)) => a == b,
            _ => false,
        }
    }
}
impl PartialOrd for EvalValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (EvalValue::Int(a), EvalValue::Int(b)) => a.partial_cmp(b),
            (EvalValue::Float(a), EvalValue::Float(b)) => a.partial_cmp(b),
            (EvalValue::Int(a), EvalValue::Float(b)) => cmp_int_float(*a, *b).into(),
            (EvalValue::Float(a), EvalValue::Int(b)) => cmp_int_float(*b, *a).reverse().into(),
            (EvalValue::String(a), EvalValue::String(b)) => a.partial_cmp(b),
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a.partial_cmp(b),
            (EvalValue::Null, EvalValue::Null) => Some(Ordering::Equal),
            (EvalValue::Array(a), EvalValue::Array(b)) => a.partial_cmp(b),
            (EvalValue::Int(a), EvalValue::Bool(true)) => (*a).partial_cmp(&1),
            (EvalValue::Int(a), EvalValue::Bool(false)) => (*a).partial_cmp(&0),
            (EvalValue::Bool(true), EvalValue::Int(b)) => (1).partial_cmp(b),
            (EvalValue::Bool(false), EvalValue::Int(b)) => (0).partial_cmp(b),
            (EvalValue::Float(a), EvalValue::Bool(true)) => (*a).partial_cmp(&1.0),
            (EvalValue::Float(a), EvalValue::Bool(false)) => (*a).partial_cmp(&0.0),
            (EvalValue::Bool(true), EvalValue::Float(b)) => (1.0).partial_cmp(b),
            (EvalValue::Bool(false), EvalValue::Float(b)) => (0.0).partial_cmp(b),
            _ => None,
        }
    }
}
impl PartialOrd<i64> for EvalValue {
    fn partial_cmp(&self, other: &i64) -> Option<Ordering> {
        match self {
            EvalValue::Int(a) => a.partial_cmp(other),
            EvalValue::Float(a) => cmp_int_float(*other, *a).reverse().into(),
            EvalValue::Bool(true) => (1).partial_cmp(other),
            EvalValue::Bool(false) => (0).partial_cmp(other),
            _ => None,
        }
    }
}
impl PartialOrd<EvalValue> for i64 {
    fn partial_cmp(&self, other: &EvalValue) -> Option<Ordering> {
        return other.partial_cmp(self).map(|o| o.reverse());
    }
}
impl PartialEq<EvalValue> for i64 {
    fn eq(&self, other: &EvalValue) -> bool {
        other == self
    }
}
impl PartialEq<i64> for EvalValue {
    fn eq(&self, other: &i64) -> bool {
        match self {
            EvalValue::Int(a) => a == other,
            EvalValue::Float(a) => cmp_int_float(*other, *a) == Ordering::Equal,
            EvalValue::Bool(true) => *other != 0,
            EvalValue::Bool(false) => *other == 0,
            _ => false,
        }
    }
}
impl PartialOrd<f64> for EvalValue {
    fn partial_cmp(&self, other: &f64) -> Option<Ordering> {
        match self {
            EvalValue::Int(a) => cmp_int_float(*a, *other).into(),
            EvalValue::Float(a) => a.partial_cmp(other),
            EvalValue::Bool(true) => (1.0).partial_cmp(other),
            EvalValue::Bool(false) => (0.0).partial_cmp(other),
            _ => None,
        }
    }
}
impl PartialEq<f64> for EvalValue {
    fn eq(&self, other: &f64) -> bool {
        match self {
            EvalValue::Int(a) => cmp_int_float(*a, *other) == Ordering::Equal,
            EvalValue::Float(a) => *a == *other,
            EvalValue::Bool(true) => *other != 0.0,
            EvalValue::Bool(false) => *other == 0.0,
            _ => false,
        }
    }
}
impl PartialOrd<EvalValue> for f64 {
    fn partial_cmp(&self, other: &EvalValue) -> Option<Ordering> {
        return other.partial_cmp(self).map(|o| o.reverse());
    }
}
impl PartialEq<EvalValue> for f64 {
    fn eq(&self, other: &EvalValue) -> bool {
        other == self
    }
}
impl PartialOrd<bool> for EvalValue {
    fn partial_cmp(&self, other: &bool) -> Option<Ordering> {
        match self {
            EvalValue::Bool(a) => a.partial_cmp(other),
            EvalValue::Int(a) => (*a != 0).partial_cmp(other),
            EvalValue::Float(a) => (*a != 0.0).partial_cmp(other),
            _ => None,
        }
    }
}
impl PartialEq<bool> for EvalValue {
    fn eq(&self, other: &bool) -> bool {
        match self {
            EvalValue::Bool(a) => a == other,
            EvalValue::Int(a) => (*a != 0) == *other,
            EvalValue::Float(a) => (*a != 0.0) == *other,
            _ => false,
        }
    }
}
impl PartialOrd<EvalValue> for bool {
    fn partial_cmp(&self, other: &EvalValue) -> Option<Ordering> {
        return other.partial_cmp(self).map(|o| o.reverse());
    }
}
impl PartialEq<EvalValue> for bool {
    fn eq(&self, other: &EvalValue) -> bool {
        other == self
    }
}
impl PartialOrd<str> for EvalValue {
    fn partial_cmp(&self, other: &str) -> Option<Ordering> {
        match self {
            EvalValue::String(a) => (**a).partial_cmp(other),
            _ => None,
        }
    }
}
impl PartialEq<str> for EvalValue {
    fn eq(&self, other: &str) -> bool {
        match self {
            EvalValue::String(a) => **a == *other,
            _ => false,
        }
    }
}
impl PartialOrd<EvalValue> for str {
    fn partial_cmp(&self, other: &EvalValue) -> Option<Ordering> {
        return other.partial_cmp(self).map(|o| o.reverse());
    }
}
impl PartialEq<EvalValue> for str {
    fn eq(&self, other: &EvalValue) -> bool {
        other == self
    }
}
impl std::fmt::Display for EvalValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalValue::Null => write!(f, "null"),
            EvalValue::Bool(value) => write!(f, "{}", value),
            EvalValue::Int(value) => write!(f, "{}", value),
            EvalValue::Float(value) => {
                if value.is_finite() && value.fract() == 0.0 {
                    write!(f, "{value:.1}")
                } else {
                    write!(f, "{value}")
                }
            },
            EvalValue::String(value) => write!(f, "\"{}\"", value),
            EvalValue::Array(values) => {
                let strings: Vec<String> = values.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", strings.join(", "))
            }
            EvalValue::Object(values) => {
                let strings: Vec<String> = values
                    .iter()
                    .map(|(k, v)| format!("\"{}\": {}", k, v))
                    .collect();
                write!(f, "{{{}}}", strings.join(", "))
            }
            EvalValue::Func(func) => write!(f, "{func}"),
        }
    }
}

/// Formats positional arguments for a call-site error without losing their values or types.
fn diagnostic_arguments(arguments: &[EvalValue]) -> String {
    if arguments.is_empty() {
        return "no arguments".to_owned();
    }
    format!(
        "arguments [{}]",
        arguments
            .iter()
            .enumerate()
            .map(|(index, value)| format!("{}: {}", index + 1, value.diagnostic()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Produces a compact source-like representation of an AST node for call-stack context.
fn diagnostic_expression(expression: &Expr) -> String {
    let rendered = expression.to_string().replace('\n', "\\n");
    let mut characters = rendered.chars();
    let shortened = characters.by_ref().take(240).collect::<String>();
    if characters.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}
impl<'de> Deserialize<'de> for EvalValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let deser = EvalValueSerializer::deserialize(deserializer)?;
        Self::try_from(deser).map_err(|err| <D as serde::Deserializer<'de>>::Error::custom(err))
    }
}
impl Serialize for EvalValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        EvalValueSerializer::from(self).serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvalValueSerializer {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Nan,
    PosInf,
    NegInf,
    String(String),
    Array(Box<[EvalValueSerializer]>),
    Object(Box<IndexMap<String, EvalValueSerializer>>),
    Func(Box<FunctionSerializer>),
}
impl From<&EvalValue> for EvalValueSerializer {
    fn from(value: &EvalValue) -> Self {
        match value {
            EvalValue::Null => EvalValueSerializer::Null,
            EvalValue::Bool(b) => EvalValueSerializer::Bool(*b),
            EvalValue::Int(i) => EvalValueSerializer::Int(*i),
            EvalValue::Float(f) => {
                if f.is_nan() {
                    return EvalValueSerializer::Nan;
                }
                if f.is_infinite() {
                    if f.is_sign_positive() {
                        return EvalValueSerializer::PosInf;
                    } else {
                        return EvalValueSerializer::NegInf;
                    }
                }
                EvalValueSerializer::Float(*f)
            },
            EvalValue::String(s) => EvalValueSerializer::String(s.to_string()),
            EvalValue::Array(arr) => {
                let serialized_arr: Box<[EvalValueSerializer]> =
                    arr.iter().map(EvalValueSerializer::from).collect::<Box<[EvalValueSerializer]>>();
                EvalValueSerializer::Array(serialized_arr)
            }
            EvalValue::Object(obj) => {
                let serialized_obj: Box<IndexMap<String, EvalValueSerializer>> =
                    obj.iter()
                        .map(|(k, v)| (k.to_string(), EvalValueSerializer::from(v)))
                        .collect::<IndexMap<String, EvalValueSerializer>>().into();
                EvalValueSerializer::Object(serialized_obj)
            }
            EvalValue::Func(func) => EvalValueSerializer::Func(Box::new(FunctionSerializer::from(func.as_ref()))),
        }
    }
}
impl TryFrom<EvalValueSerializer> for EvalValue {
    type Error = crate::err::Error;
    fn try_from(value: EvalValueSerializer) -> Result<Self, crate::err::Error> {
        Ok(match value {
            EvalValueSerializer::Null => EvalValue::Null,
            EvalValueSerializer::Bool(b) => EvalValue::Bool(b),
            EvalValueSerializer::Int(i) => EvalValue::Int(i),
            EvalValueSerializer::Float(f) => EvalValue::Float(f),
            EvalValueSerializer::Nan => EvalValue::Float(f64::NAN),
            EvalValueSerializer::PosInf => EvalValue::Float(f64::INFINITY),
            EvalValueSerializer::NegInf => EvalValue::Float(f64::NEG_INFINITY),
            EvalValueSerializer::String(s) => EvalValue::String(Arc::from(s)),
            EvalValueSerializer::Array(arr) => {
                let deserialized_arr: Arc<[EvalValue]> =
                    arr.into_vec().into_iter().map(|value| EvalValue::try_from(value)).collect::<Result<Vec<EvalValue>, crate::err::Error>>()?.into();
                EvalValue::Array(deserialized_arr)
            }
            EvalValueSerializer::Object(obj) => {
                let deserialized_obj: Arc<IndexMap<Arc<str>, EvalValue>> =
                    obj.into_iter()
                        .map(|(k, v)| Ok((Arc::from(k.as_str()), EvalValue::try_from(v)?)))
                        .collect::<Result<IndexMap<Arc<str>, EvalValue>, crate::err::Error>>()?.into();
                EvalValue::Object(deserialized_obj)
            }
            EvalValueSerializer::Func(func) => {
                let func = Function::try_from(*func)?;
                EvalValue::Func(Arc::new(func))
            }
        })
    }
}

/// Classifies formula evaluation and conversion failures.
#[derive(Clone, Debug, PartialEq)]
pub enum EvalError {
    InvalidNumber,
    NonSerializableValue,
}
impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::InvalidNumber => write!(f, "Invalid number"),
            EvalError::NonSerializableValue => write!(f, "Value cannot be serialized"),
        }
    }
}
impl std::error::Error for EvalError {}

impl EvalValue {

    /// Returns whether this value is considered true by formula semantics.
    pub fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(value) => *value,
            Self::Int(value) => *value != 0,
            Self::Float(value) => *value != 0.0,
            Self::String(value) => !value.is_empty(),
            Self::Array(value) => !value.is_empty(),
            Self::Object(value) => !value.is_empty(),
            Self::Func(_) => true,
        }
    }

    /// Describes a runtime value for an error message without flooding the terminal.
    ///
    /// The rendered value is escaped and capped at 180 characters, then followed by its
    /// formula-language type. This makes type errors identify the actual offending value.
    pub fn diagnostic(&self) -> String {
        let rendered = self
            .repr()
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        let mut characters = rendered.chars();
        let shortened = characters.by_ref().take(180).collect::<String>();
        let rendered = if characters.next().is_some() {
            format!("{shortened}…")
        } else {
            shortened
        };
        format!("{rendered} (type {})", self.type_name())
    }
    /// Converts integers, floats, booleans, and numeric strings to `i64`.
    ///
    /// Floats are truncated using Rust's numeric cast semantics.
    ///
    /// # Errors
    ///
    /// Returns an error for non-numeric strings and unsupported value types.
    pub fn to_i64(&self) -> Result<i64> {
        match self {
            Self::Int(value) => Ok(*value),
            Self::Float(value) => Ok(*value as i64),
            Self::Bool(value) => Ok(if *value { 1 } else { 0 }),
            Self::String(value) => value.parse::<i64>().map_err(|source| {
                Error::InvalidInteger { value: self.diagnostic(), source }
            }),
            _ => bail!(Type,
                "Cannot convert {} to an integer: only int, float, bool, and numeric strings are accepted",
                self.diagnostic()
            ),
        }
    }
    /// Converts integers, floats, booleans, and numeric strings to `f64`.
    ///
    /// # Errors
    ///
    /// Returns an error for non-numeric strings and unsupported value types.
    pub fn to_f64(&self) -> Result<f64> {
        match self {
            Self::Int(value) => Ok(*value as f64),
            Self::Float(value) => Ok(*value),
            Self::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
            Self::String(value) => value.parse::<f64>().map_err(|source| {
                Error::InvalidFloat { value: self.diagnostic(), source }
            }),
            _ => bail!(Type,
                "Cannot convert {} to a number: only int, float, bool, and numeric strings are accepted",
                self.diagnostic()
            ),
        }
    }

    /// Formats the value using the string conventions of the formula language.
    pub fn repr(&self) -> Arc<str> {
        match self {
            Self::Null => "null".into(),
            Self::Bool(true) => "true".into(),
            Self::Bool(false) => "false".into(),
            Self::Int(value) => value.to_string().into(),
            Self::Float(value) => {
                if value.is_nan() {
                    "nan".into()
                } else if value.is_infinite() {
                    if value.is_sign_negative() {
                        "-inf".into()
                    } else {
                        "inf".into()
                    }
                } else {
                    value.to_string().into()
                }
            }
            Self::String(value) => value.clone(),
            Self::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(|value| match value {
                        Self::String(text) => format!("'{text}'").into(),
                        _ => value.repr(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into(),
            Self::Object(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(|(key, value)| format!("'{key}': {}", value.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into(),
            Self::Func(_) => "<function>".into(),
        }
    }

    /// Returns the formula-language name of this value type.
    pub fn type_name(&self) -> Arc<str> {
        match self {
            Self::Null => NULL.clone(),
            Self::Bool(_) => BOOL.clone(),
            Self::Int(_) => INT.clone(),
            Self::Float(_) => FLOAT.clone(),
            Self::String(_) => STR.clone(),
            Self::Array(_) => LIST.clone(),
            Self::Object(_) => DICT.clone(),
            Self::Func(_) => FUNCTION.clone(),
        }
    }

    /// Returns a direct data member without consulting the type's method registry.
    ///
    /// Dictionary keys use string members. Lists and strings accept
    /// integer indexes (including negative indexes); the dotted numeric form is accepted for
    /// lists for compatibility with `liste.0`.
    fn direct_member(
        &self,
        member: &EvalValue,
    ) -> Result<Option<EvalValue>> {
        match (self, member) {
            (Self::Object(values), Self::String(key)) => Ok(values.get(key).cloned()),
            /*
            (Self::Object(values), Self::Int(key)) => {
                Ok(values.get(key.to_string().as_str()).cloned())
            }
            */
            (Self::Array(values), Self::Int(index)) => {
                let index = normalize_index(*index, values.len())?;
                Ok(values.get(index).cloned())
            }
            /*
            (Self::Array(values), Self::String(index)) => {
                let Ok(index) = index.parse::<i64>() else {
                    return Ok(None);
                };
                let index = normalize_index(index, values.len())?;
                Ok(values.get(index).cloned())
            }
            */
            (Self::String(value), Self::Int(index)) => {
                let characters = value.chars().collect::<Vec<_>>();
                let index = normalize_index(*index, characters.len())?;
                Ok(characters
                    .get(index)
                    .map(|character| Self::String(character.to_string().into())))
            }
            _ => Ok(None),
        }
    }

    /// Returns the registry of methods exposed by this value type.
    fn method_registry(&self) -> Option<&'static IndexMap<Arc<str>, EvalValue>> {
        match self {
            Self::Array(_) => Some(&*builtins::ARRAY_METHODS),
            Self::Object(_) => Some(&*builtins::OBJECT_METHODS),
            Self::String(_) => Some(&*builtins::STRING_METHODS),
            Self::Int(_) => Some(&*builtins::INT_METHODS),
            Self::Float(_) => Some(&*builtins::FLOAT_METHODS),
            _ => None,
        }
    }

    /// Resolves one dictionary key, indexed item, or registered method.
    ///
    /// Direct data always takes precedence over a method with the same name. Only strings and
    /// integers may be used as members. Method values are returned as ordinary
    /// [`EvalValue::Func`] values; the evaluator supplies the receiver when a registered method
    /// is called with postfix syntax.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported member types, invalid indexes, or unknown members.
    pub fn get_member(
        &self,
        member: &EvalValue,
    ) -> Result<EvalValue> {
        if !matches!(member, Self::String(_) | Self::Int(_)) {
            bail!(Type,
                "Cannot access member {} of {}: a member must be a string or an integer",
                member.diagnostic(),
                self.diagnostic()
            )
        }
        if let Some(value) = self.direct_member(member)? {
            return Ok(value);
        }
        if let Self::String(name) = member {
            if let Some(method) = self
                .method_registry()
                .and_then(|registry| registry.get(name.as_ref()))
            {
                return Ok(method.clone());
            }
        }
        Err(Error::UnknownMember { member: member.diagnostic(), value: self.diagnostic() })
    }

    /// Calls this value as a built-in or user-defined function.
    ///
    /// User-defined functions accept extra positional arguments, expose the complete list as
    /// `args`, and bind the leading arguments to their declared parameter names. Their lexical
    /// captures are combined with the dynamic runtime portion of the caller's environment.
    ///
    /// # Errors
    ///
    /// Returns an error when this value is not callable, when too few arguments are supplied,
    /// when the shared function-call depth limit is reached, or when the called function fails.
    pub fn call(
        self,
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        match self {
            Self::Func(function) => match function.as_ref() {
                Function::Builtin(name, function) => formula
                    .with_call_frame(name.clone(), |formula| function(arguments, formula, env))
                    .with_context(|| {
                        format!(
                            "Call to builtin `{name}` failed with {}",
                            diagnostic_arguments(arguments)
                        )
                    }),
                Function::User(function) => {
                    let name: Arc<str> = format!(
                        "fn({})",
                        function.definition.parameters.as_ref()
                            .map(|parameters| format_patterns(parameters))
                            .unwrap_or_default()
                    ).into();
                    formula
                        .with_call_frame(name.clone(), |formula| {
                            formula.call_user_function(function.clone(), arguments, env, &name)
                        })
                        .with_context(|| {
                            format!(
                                "Function call failed with {}",
                                diagnostic_arguments(arguments)
                            )
                        })
                },
            },
            value => bail!(Type,
                "Cannot call {}: this value is not a function",
                value.diagnostic()
            ),
        }
    }

    /// Attempts to add using formula-language semantics.
    ///
    /// Implements formula-language coercion rules and rejects unsupported operand types or
    /// arithmetic domain errors instead of falling back to string conversion.
    ///
    /// # Errors
    ///
    /// Returns an error when the operand types do not support addition.
    pub fn try_add(&self, other: &Self) -> Result<Self> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Ok(Self::Int(a.checked_add(*b).ok_or_else(|| Error::Overflow("Integer addition overflow".into()))?)),
            (Self::Int(a), Self::Bool(b)) => Ok(Self::Bool((*a != 0) || *b)),
            (Self::Bool(a), Self::Int(b)) => Ok(Self::Bool(*a || (*b != 0))),
            (Self::Bool(a), Self::Bool(b)) => Ok(Self::Bool(*a || *b)),
            (Self::Float(a), Self::Float(b)) => Ok(Self::Float(a + b)),
            (Self::Int(a), Self::Float(b)) => Ok(Self::Float((*a as f64) + b)),
            (Self::Float(a), Self::Int(b)) => Ok(Self::Float(a + (*b as f64))),
            (Self::String(a), Self::String(b)) => Ok(Self::String(format!("{a}{b}").into()).into()),
            (Self::Array(a), Self::Array(b)) => {
                Ok(Self::Array(a.iter().chain(b.iter()).cloned().collect()))
            }
            (Self::Object(a), Self::Object(b)) => {
                let mut merged = a.clone();
                Arc::make_mut(&mut merged).extend((**b).clone());
                Ok(Self::Object(merged))
            }
            _ => bail!(Type,
                "Cannot add {} and {}: both values must be compatible numbers, two strings, two lists, or two dictionaries",
                self.diagnostic(),
                other.diagnostic()
            ),
        }
    }
    /// Attempts to sub using formula-language semantics.
    ///
    /// Implements formula-language coercion rules and rejects unsupported operand types or
    /// arithmetic domain errors instead of falling back to string conversion.
    ///
    /// # Errors
    ///
    /// Returns an error unless both operands are numeric.
    pub fn try_sub(&self, other: &Self) -> Result<Self> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Ok(Self::Int(a.checked_sub(*b).ok_or_else(|| Error::Overflow("Integer subtraction overflow".into()))?)),
            (Self::Float(a), Self::Float(b)) => Ok(Self::Float(a - b)),
            (Self::Int(a), Self::Float(b)) => Ok(Self::Float((*a as f64) - b)),
            (Self::Float(a), Self::Int(b)) => Ok(Self::Float(a - (*b as f64))),
            _ => bail!(Type,
                "Cannot subtract {} and {}: both values must be numeric",
                self.diagnostic(),
                other.diagnostic()
            ),
        }
    }
    /// Attempts to mul using formula-language semantics.
    ///
    /// Implements formula-language coercion rules and rejects unsupported operand types or
    /// arithmetic domain errors instead of falling back to string conversion.
    ///
    /// # Errors
    ///
    /// Returns an error when the operand types do not support multiplication.
    pub fn try_mul(&self, other: &Self) -> Result<Self> {
        match (self, other) {
            (Self::Int(a), Self::Bool(b)) => Ok(Self::Bool((*a != 0) && *b)),
            (Self::Bool(a), Self::Int(b)) => Ok(Self::Bool(*a && (*b != 0))),
            (Self::Bool(a), Self::Bool(b)) => Ok(Self::Bool(*a && *b)),
            (Self::Int(a), Self::Int(b)) => Ok(Self::Int(a.checked_mul(*b).ok_or_else(|| Error::Overflow("Integer multiplication overflow".into()))?)),
            (Self::Float(a), Self::Float(b)) => Ok(Self::Float(a * b)),
            (Self::Int(a), Self::Float(b)) => Ok(Self::Float((*a as f64) * b)),
            (Self::Float(a), Self::Int(b)) => Ok(Self::Float(a * (*b as f64))),
            (Self::String(a), Self::Int(b)) => {
                if *b < 0 {
                    bail!(InvalidArgument,
                        "Cannot multiply {} by {}: a string repetition count cannot be negative",
                        self.diagnostic(),
                        other.diagnostic()
                    );
                }
                if *b == 1 {
                    Ok(Self::String(a.clone()))
                } else {
                    Ok(Self::String((*a).repeat(*b as usize).into()))
                }
            }
            (Self::Int(a), Self::String(b)) => {
                if *a < 0 {
                    bail!(InvalidArgument,
                        "Cannot multiply {} by {}: a string repetition count cannot be negative",
                        self.diagnostic(),
                        other.diagnostic()
                    );
                }
                if *a == 1 {
                    Ok(Self::String(b.clone()))
                } else {
                    Ok(Self::String((*b).repeat(*a as usize).into()))
                }
            }
            _ => bail!(Type,
                "Cannot multiply {} and {}: both values must be numeric, or one must be a string and the other an integer",
                self.diagnostic(),
                other.diagnostic()
            ),
        }
    }
    /// Attempts to div using formula-language semantics.
    ///
    /// Implements formula-language coercion rules and rejects unsupported operand types or
    /// arithmetic domain errors instead of falling back to string conversion.
    ///
    /// # Errors
    ///
    /// Returns an error for non-numeric operands or division by zero.
    pub fn try_div(&self, other: &Self) -> Result<Self> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => {
                if *b == 0 {
                    bail!(DivisionByZero, "Cannot divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float((*a as f64) / (*b as f64)))
            }
            (Self::Float(a), Self::Float(b)) => {
                if *b == 0.0 {
                    bail!(DivisionByZero, "Cannot divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float(a / b))
            }
            (Self::Int(a), Self::Float(b)) => {
                if *b == 0.0 {
                    bail!(DivisionByZero, "Cannot divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float((*a as f64) / b))
            }
            (Self::Float(a), Self::Int(b)) => {
                if *b == 0 {
                    bail!(DivisionByZero, "Cannot divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float(a / (*b as f64)))
            }
            _ => bail!(Type,
                "Cannot divide {} by {}: both values must be numeric",
                self.diagnostic(),
                other.diagnostic()
            ),
        }
    }
    /// Attempts to mod using formula-language semantics.
    ///
    /// Implements formula-language coercion rules and rejects unsupported operand types or
    /// arithmetic domain errors instead of falling back to string conversion.
    ///
    /// # Errors
    ///
    /// Returns an error for non-numeric operands or a zero divisor.
    pub fn try_mod(&self, other: &Self) -> Result<Self> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => {
                if *b == 0 {
                    bail!(DivisionByZero, "Cannot compute {} modulo {}: modulo by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Int(a.checked_rem(*b).ok_or_else(|| Error::Overflow("Overflow in integer mod operation".into()))?))
            }
            (Self::Float(a), Self::Float(b)) => {
                if *b == 0.0 {
                    bail!(DivisionByZero, "Cannot compute {} modulo {}: modulo by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float(a - (a / b).floor() * b))
            }
            (Self::Int(a), Self::Float(b)) => {
                if *b == 0.0 {
                    bail!(DivisionByZero, "Cannot compute {} modulo {}: modulo by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float((*a as f64) % b))
            }
            (Self::Float(a), Self::Int(b)) => {
                if *b == 0 {
                    bail!(DivisionByZero, "Cannot compute {} modulo {}: modulo by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float(a % (*b as f64)))
            }
            _ => bail!(Type,
                "Cannot compute the remainder of {} and {}: both values must be numeric",
                self.diagnostic(),
                other.diagnostic()
            ),
        }
    }
    /// Attempts to floor div using formula-language semantics.
    ///
    /// Implements formula-language coercion rules and rejects unsupported operand types or
    /// arithmetic domain errors instead of falling back to string conversion.
    ///
    /// # Errors
    ///
    /// Returns an error for non-numeric operands or division by zero.
    pub fn try_floor_div(&self, other: &Self) -> Result<Self> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => {
                if *b == 0 {
                    bail!(DivisionByZero, "Cannot floor-divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                let mut quo = a.checked_div(*b).ok_or_else(|| Error::Overflow("Overflow in integer floordiv operation".into()))?;
                let rem = a.checked_rem(*b).ok_or_else(|| Error::Overflow("Overflow in floordiv operation".into()))?;
                if rem != 0 && (*a < 0) != (*b < 0) {
                    quo -= 1;
                }
                Ok(Self::Int(quo))
            }
            (Self::Float(a), Self::Float(b)) => {
                if *b == 0.0 {
                    bail!(DivisionByZero, "Cannot floor-divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float((a / b).floor()))
            }
            (Self::Int(a), Self::Float(b)) => {
                if *b == 0.0 {
                    bail!(DivisionByZero, "Cannot floor-divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float(((*a as f64) / b).floor()))
            }
            (Self::Float(a), Self::Int(b)) => {
                if *b == 0 {
                    bail!(DivisionByZero, "Cannot floor-divide {} by {}: division by zero", self.diagnostic(), other.diagnostic());
                }
                Ok(Self::Float((a / (*b as f64)).floor()))
            }
            _ => bail!(Type,
                "Cannot floor-divide {} by {}: both values must be numeric",
                self.diagnostic(),
                other.diagnostic()
            ),
        }
    }

    /// Define a total ordering comparison between two `EvalValue` instances.
    /// 
    /// Values of different types are compared according to a predefined ordering:
    /// null < int, bool and float < string < array < dict < func
    /// 
    /// lists and dictionaries are compared lexicographically based on their elements or key-value pairs.
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (EvalValue::Int(a), EvalValue::Int(b)) => a.cmp(b),
            (EvalValue::Float(a), EvalValue::Float(b)) => a.total_cmp(b),
            (EvalValue::Int(a), EvalValue::Float(b)) => cmp_int_float(*a, *b).into(),
            (EvalValue::Float(a), EvalValue::Int(b)) => cmp_int_float(*b, *a).reverse().into(),
            (EvalValue::String(a), EvalValue::String(b)) => a.cmp(b),
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a.cmp(b),
            (EvalValue::Null, EvalValue::Null) => Ordering::Equal,
            (EvalValue::Array(a), EvalValue::Array(b)) => {
                let len_cmp = a.len().cmp(&b.len());
                if len_cmp != Ordering::Equal {
                    return len_cmp;
                }
                for (item_a, item_b) in a.iter().zip(b.iter()) {
                    let cmp = item_a.total_cmp(item_b);
                    if cmp != Ordering::Equal {
                        return cmp;
                    }
                }
                Ordering::Equal
            },
            (EvalValue::Object(a), EvalValue::Object(b)) => {
                let mut entries_a: Vec<_> = a.iter().collect();
                let mut entries_b: Vec<_> = b.iter().collect();
                entries_a.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
                entries_b.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
                for ((k1, v1), (k2, v2)) in entries_a.iter().zip(entries_b.iter()) {
                    let key_cmp = k1.cmp(k2);
                    if key_cmp != Ordering::Equal {
                        return key_cmp;
                    }
                    let value_cmp = v1.total_cmp(v2);
                    if value_cmp != Ordering::Equal {
                        return value_cmp;
                    }
                }
                entries_a.len().cmp(&entries_b.len())
            },
            (EvalValue::Int(a), EvalValue::Bool(true)) => (*a).cmp(&1),
            (EvalValue::Int(a), EvalValue::Bool(false)) => (*a).cmp(&0),
            (EvalValue::Bool(true), EvalValue::Int(b)) => (1).cmp(b),
            (EvalValue::Bool(false), EvalValue::Int(b)) => (0).cmp(b),
            (EvalValue::Float(a), EvalValue::Bool(true)) => (*a).total_cmp(&1.0),
            (EvalValue::Float(a), EvalValue::Bool(false)) => (*a).total_cmp(&0.0),
            (EvalValue::Bool(true), EvalValue::Float(b)) => (1.0f64).total_cmp(b),
            (EvalValue::Bool(false), EvalValue::Float(b)) => (0.0f64).total_cmp(b),
            (EvalValue::Null, _) => Ordering::Less,
            (_, EvalValue::Null) => Ordering::Greater,
            (EvalValue::Int(_), _) => Ordering::Less,
            (_, EvalValue::Int(_)) => Ordering::Greater,
            (EvalValue::Bool(_), _) => Ordering::Less,
            (_, EvalValue::Bool(_)) => Ordering::Greater,
            (EvalValue::Float(_), _) => Ordering::Less,
            (_, EvalValue::Float(_)) => Ordering::Greater,
            (EvalValue::String(_), _) => Ordering::Less,
            (_, EvalValue::String(_)) => Ordering::Greater,
            (EvalValue::Array(_), _) => Ordering::Less,
            (_, EvalValue::Array(_)) => Ordering::Greater,
            (EvalValue::Object(_), _) => Ordering::Less,
            (_, EvalValue::Object(_)) => Ordering::Greater,
            (EvalValue::Func(_), EvalValue::Func(_)) => Ordering::Equal,
        }
    }
    /// Strict equality comparison.
    /// Returns true if `self` and `other` are of the same type and have the same value.
    pub fn strict_eq(&self, other: &EvalValue) -> bool {
        match (self, other) {
            (EvalValue::Null, EvalValue::Null) => true,
            (EvalValue::Int(a), EvalValue::Int(b)) => a == b,
            (EvalValue::Float(a), EvalValue::Float(b)) => a == b,
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a == b,
            (EvalValue::String(a), EvalValue::String(b)) => a == b,
            (EvalValue::Array(a), EvalValue::Array(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for (item_a, item_b) in a.iter().zip(b.iter()) {
                    if !item_a.strict_eq(item_b) {
                        return false;
                    }
                }
                true
            },
            (EvalValue::Object(a), EvalValue::Object(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for (key, value_a) in a.iter() {
                    match b.get(key) {
                        Some(value_b) => {
                            if !value_a.strict_eq(value_b) {
                                return false;
                            }
                        },
                        None => return false,
                    }
                }
                true
            },
            (EvalValue::Func(a), EvalValue::Func(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub fn func(name: &str, func: fn(&[EvalValue], &mut FormulaEngine, &Env) -> Result<EvalValue>) -> (Arc<str>, Self) {
        let key: Arc<str> = Arc::from(name);
        (key.clone(), EvalValue::Func(Function::Builtin(key, func).into()))
    }
}

impl From<String> for EvalValue {
    fn from(value: String) -> Self {
        EvalValue::String(value.into())
    }
}
impl From<Arc<str>> for EvalValue {
    fn from(value: Arc<str>) -> Self {
        EvalValue::String(value)
    }
}
impl From<&str> for EvalValue {
    fn from(value: &str) -> Self {
        EvalValue::String(value.into())
    }
}
impl From<i64> for EvalValue {
    fn from(value: i64) -> Self {
        EvalValue::Int(value)
    }
}
impl From<f64> for EvalValue {
    fn from(value: f64) -> Self {
        EvalValue::Float(value)
    }
}
impl From<bool> for EvalValue {
    fn from(value: bool) -> Self {
        EvalValue::Bool(value)
    }
}
impl TryFrom<EvalValue> for Arc<str> {
    type Error = crate::err::Error;

    fn try_from(value: EvalValue) -> Result<Self, Self::Error> {
        match value {
            EvalValue::String(s) => Ok(s),
            other => Err(error!(Type,
                "Cannot use {} as a string: a value of type str is required",
                other.diagnostic()
            )),
        }
    }
}
impl TryFrom<EvalValue> for String {
    type Error = crate::err::Error;

    fn try_from(value: EvalValue) -> Result<Self, Self::Error> {
        match value {
            EvalValue::String(s) => Ok(s.to_string()),
            other => Err(error!(Type,
                "Cannot use {} as a string: a value of type str is required",
                other.diagnostic()
            )),
        }
    }
}
impl TryFrom<EvalValue> for i64 {
    type Error = crate::err::Error;

    fn try_from(value: EvalValue) -> Result<Self, Self::Error> {
        match value {
            EvalValue::Int(i) => Ok(i),
            other => Err(error!(Type,
                "Cannot use {} as a strict integer: a value of type int is required",
                other.diagnostic()
            )),
        }
    }
}
impl TryFrom<EvalValue> for f64 {
    type Error = crate::err::Error;

    fn try_from(value: EvalValue) -> Result<Self, Self::Error> {
        match value {
            EvalValue::Float(f) => Ok(f),
            other => Err(error!(Type,
                "Cannot use {} as a strict float: a value of type float is required",
                other.diagnostic()
            )),
        }
    }
}
impl TryFrom<EvalValue> for bool {
    type Error = crate::err::Error;

    fn try_from(value: EvalValue) -> Result<Self, Self::Error> {
        match value {
            EvalValue::Bool(b) => Ok(b),
            other => Err(error!(Type,
                "Cannot use {} as a strict boolean: a value of type bool is required",
                other.diagnostic()
            )),
        }
    }
}

/// Origin of the binding selected by environment lookup, independent of its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingOrigin {
    Local,
    EngineGlobal,
}

/// Stores named values exposed to formula evaluation.
#[derive(Clone, Debug, Default)]
pub struct Env<'a> {
    parent: Vec<&'a Env<'a>>,
    pub vars: Arc<IndexMap<Arc<str>, EvalValue>>,
    /// Origins of direct global bindings. An absent entry means a local binding.
    origins: Arc<IndexMap<Arc<str>, BindingOrigin>>,
}

impl<'a> Env<'a> {
    /// Creates an empty formula environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an environment by cloning an existing map of evaluated values.
    pub fn from_index_map(values: &IndexMap<Arc<str>, EvalValue>) -> Env<'static> {
        Env {
            parent: Vec::new(),
            vars: Arc::from(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<IndexMap<_, _>>(),
            ),
            origins: Arc::new(IndexMap::new()),
        }
    }

    /// Creates an owned environment that shares an existing immutable value map.
    pub fn from_arc(values: Arc<IndexMap<Arc<str>, EvalValue>>) -> Env<'static> {
        Env {
            parent: Vec::new(),
            vars: values,
            origins: Arc::new(IndexMap::new()),
        }
    }

    /// Takes an owned snapshot and marks every visible binding as an engine global.
    /// This is done once when installing globals, including names inherited from parents.
    pub(crate) fn into_engine_globals(self) -> Env<'static> {
        let mut globals = self.solidified();
        globals.origins = Arc::new(
            globals
                .vars
                .keys()
                .map(|name| (name.clone(), BindingOrigin::EngineGlobal))
                .collect(),
        );
        globals
    }
    /// Creates a local binding, replacing any existing binding even when the value is equal.
    pub fn insert(&mut self, key: impl Into<Arc<str>>, value: EvalValue) {
        let key: Arc<str> = key.into();
        let inner = Arc::make_mut(&mut self.vars);
        inner.insert(key.clone(), value);
        Arc::make_mut(&mut self.origins).shift_remove(key.as_ref());
    }

    /// Extends this environment with values from another environment.
    pub fn extend(&mut self, values: &IndexMap<Arc<str>, EvalValue>) {
        let inner = Arc::make_mut(&mut self.vars);
        let origins = Arc::make_mut(&mut self.origins);
        for (key, value) in values {
            inner.insert(key.clone(), value.clone());
            origins.shift_remove(key.as_ref());
        }
    }

    /// Returns the value bound to `key`, or `None` when the name is undefined.
    pub fn get(&self, key: &str) -> Option<&EvalValue> {
        self.get_binding(key).map(|(value, _)| value)
    }

    /// Resolves the value and its origin together using the same lookup precedence.
    pub(crate) fn get_binding(&self, key: &str) -> Option<(&EvalValue, BindingOrigin)> {
        if let Some(value) = self.vars.get(key) {
            let origin = self
                .origins
                .get(key)
                .copied()
                .unwrap_or(BindingOrigin::Local);
            Some((value, origin))
        } else {
            self.parent
                .iter()
                .find_map(|parent| parent.get_binding(key))
        }
    }

    /// Lists visible names for an unknown-variable diagnostic.
    pub(crate) fn visible_names_for_error(&self) -> String {
        let solidified = self.visible_names_for_error_inner();
        let total = solidified.len();
        let mut names = solidified.iter()
            .take(24)
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>();
        if total > names.len() {
            names.push(format!("… and {} more", total - names.len()));
        }
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    }
    pub(crate) fn visible_names_for_error_inner(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        if !Arc::ptr_eq(&self.vars, &GLOBAL_ENV.vars) {
            for key in self.vars.keys() {
                set.insert(key.to_string());
            }
        }
        for parent in &self.parent {
            set.extend(parent.visible_names_for_error_inner());
        }
        set
    }

    /// Returns a cloned environment extended with `other`.
    ///
    /// Bindings from `other` override names already present in `self`.
    pub fn merged<'b: 'a>(&'a self, other: &'b Env<'a>) -> Env<'a> {
        let parents = vec![other, self];
        Env {
            parent: parents,
            vars: Arc::from(IndexMap::new()),
            origins: Arc::new(IndexMap::new()),
        }
    }

    /// Returns a new environment with all parent environments merged together
    pub fn merge(parents: &[&'a Env<'a>]) -> Env<'a> {
        Env {
            parent: {
                let mut p = parents.to_vec();
                p.reverse();
                p
            },
            vars: Arc::from(IndexMap::new()),
            origins: Arc::new(IndexMap::new()),
        }
    }

    /// Add a parent environment to the chain of lookups. This is useful for temporary overrides without cloning the entire environment.
    ///
    pub fn with_parent<'b>(mut self, other: &'b Env<'b>) -> Env<'a>
    where
        'b: 'a,
    {
        self.parent.push(other);
        self
    }
    /// Flattens parent environments while preserving each visible binding's origin.
    pub fn solidified(&self) -> Env<'static> {
        if self.parent.is_empty() {
            return Env {
                parent: Vec::new(),
                vars: self.vars.clone(),
                origins: self.origins.clone(),
            };
        }
        let mut solidified = Env::new();
        for parent in self.parent.iter().rev() {
            solidified = solidified.expand(parent.solidified());
        }
        solidified.expand(Env {
            parent: Vec::new(),
            vars: self.vars.clone(),
            origins: self.origins.clone(),
        })
    }

    /// Merges another environment into this one, overriding existing bindings with those from the other environment.
    pub fn expand<'b: 'a>(mut self, other: Env<'b>) -> Env<'a> {
        self.parent = [other.parent.as_slice(), self.parent.as_slice()].concat();
        let inner = Arc::make_mut(&mut self.vars);
        let origins = Arc::make_mut(&mut self.origins);
        for (key, value) in other.vars.iter() {
            inner.insert(key.clone(), value.clone());
            if let Some(origin) = other.origins.get(key.as_ref()) {
                origins.insert(key.clone(), *origin);
            } else {
                origins.shift_remove(key.as_ref());
            }
        }
        self
    }
}

/// A binding pattern used by `let`, function parameters, and comprehensions.
/// The name `_` is an ignore pattern: it consumes a value without binding a name.
/// A list may contain one `*name` rest capture at each nesting level.
/// Untagged serialization keeps existing name-only saved formulas compatible.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BindingPattern {
    Name(String),
    List(Vec<BindingPattern>),
    Rest { rest: String },
}

impl From<&str> for BindingPattern {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}
impl From<String> for BindingPattern {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}
impl From<Arc<str>> for BindingPattern {
    fn from(name: Arc<str>) -> Self {
        Self::Name(name.to_string())
    }
}
impl std::fmt::Display for BindingPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(name) => write!(f, "{name}"),
            Self::List(items) => write!(f, "[{}]", format_patterns(items)),
            Self::Rest { rest } => write!(f, "*{rest}"),
        }
    }
}
fn format_patterns(patterns: &[BindingPattern]) -> String {
    patterns
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
impl BindingPattern {
    fn collect_names(&self, names: &mut IndexSet<Arc<str>>) {
        match self {
            Self::Name(name) | Self::Rest { rest: name } => {
                if name != "_" {
                    names.insert(name.as_str().into());
                }
            }
            Self::List(items) => {
                for item in items {
                    item.collect_names(names);
                }
            }
        }
    }

    fn bind(&self, value: EvalValue, env: &mut Env) -> Result<()> {
        match self {
            Self::Name(name) => {
                if name != "_" {
                    env.insert(name.as_str(), value);
                }
            }
            Self::List(patterns) => {
                let EvalValue::Array(values) = &value else {
                    bail!(Type,
                        "Pattern {self} requires a list, got {}",
                        value.diagnostic()
                    );
                };
                let rest_count = patterns.iter()
                    .filter(|pattern| matches!(pattern, Self::Rest { .. }))
                    .count();
                if rest_count > 1 {
                    bail!(InvalidArgument, "A list pattern can contain only one rest capture");
                }
                let required = patterns.len() - rest_count;
                if values.len() < required || (rest_count == 0 && values.len() != required) {
                    bail!(InvalidArgument,
                        "Pattern {self} requires {} {required} element(s), got {}",
                        if rest_count == 1 { "at least" } else { "exactly" },
                        values.len()
                    );
                }
                let mut index = 0;
                for pattern in patterns {
                    if let Self::Rest { rest: name } = pattern {
                        let end = index + values.len() - required;
                        if name != "_" {
                            env.insert(name.as_str(), EvalValue::Array(values[index..end].into()));
                        }
                        index = end;
                    } else {
                        pattern.bind(values[index].clone(), env)?;
                        index += 1;
                    }
                }
            }
            Self::Rest { .. } => {
                bail!(InvalidArgument, "A rest capture must belong to a list pattern");
            }
        }
        Ok(())
    }
}

/// Declares the parsed definition of one user-defined formula function.
///
/// When `parameters` is `Some`, calls must provide at least that many arguments and each
/// leading value is bound using the corresponding name or list pattern. Every call also exposes
/// the complete argument list through `args`. The body uses ordinary formula syntax and is
/// evaluated once per call in the closure environment extended with those bindings.
#[derive(Clone, Debug, PartialEq)]
pub struct FormulaFunction {
    /// Ordered parameter patterns, or `None` when the function accepts any arity through `args`.
    pub parameters: Option<Arc<[BindingPattern]>>,
    /// Complete `${...}` formula resolved once for each call.
    pub body: Arc<Expr>,
}
impl std::fmt::Display for FormulaFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(parameters) = &self.parameters {
            write!(f, "fn {} -> {}", format_patterns(parameters), self.body)
        } else {
            write!(f, "fn -> {}", self.body)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaFunctionSerializer {
    parameters: Option<Vec<BindingPattern>>,
    body: Box<ExprSerializer>,
}
impl From<&FormulaFunction> for FormulaFunctionSerializer {
    fn from(function: &FormulaFunction) -> Self {
        Self {
            parameters: function
                .parameters
                .as_ref()
                .map(|params| params.to_vec()),
            body: Box::new(ExprSerializer::from(function.body.as_ref())),
        }
    }
}
impl TryFrom<FormulaFunctionSerializer> for FormulaFunction {
    type Error = crate::err::Error;
    fn try_from(serializer: FormulaFunctionSerializer) -> Result<Self, Self::Error> {
        Ok(Self {
            parameters: serializer.parameters.map(Arc::from),
            body: Arc::new((*serializer.body).try_into()?),
        })
    }
}

/// Stores a function definition together with the lexical values captured when it was created.
///
/// Only referenced local values are captured. Engine globals remain dynamic and are
/// resolved at call time.
#[derive(Clone, Debug, PartialEq)]
pub struct FormulaClosure {
    /// Parsed function definition shared with the cached formula AST.
    pub definition: FormulaFunction,
    /// Free lexical bindings copied by value when the function expression was evaluated.
    pub captures: Arc<IndexMap<Arc<str>, EvalValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaClosureSerializer {
    definition: FormulaFunctionSerializer,
    captures: IndexMap<String, EvalValue>,
}
impl From<&FormulaClosure> for FormulaClosureSerializer {
    fn from(closure: &FormulaClosure) -> Self {
        Self {
            definition: FormulaFunctionSerializer::from(&closure.definition),
            captures: closure
                .captures
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
        }
    }
}
impl TryFrom<FormulaClosureSerializer> for FormulaClosure {
    type Error = crate::err::Error;
    fn try_from(serializer: FormulaClosureSerializer) -> Result<Self, Self::Error> {
        Ok(Self {
            definition: serializer.definition.try_into()?,
            captures: Arc::from(
                serializer
                    .captures
                    .into_iter()
                    .map(|(key, value)| (Arc::from(key), value))
                    .collect::<IndexMap<_, _>>(),
            ),
        })
    }
}

impl FormulaClosure {
    /// Wraps a function definition that has no lexical creation environment.
    pub fn without_captures(definition: FormulaFunction) -> Self {
        Self {
            definition,
            captures: Arc::new(IndexMap::new()),
        }
    }
}
impl std::fmt::Display for FormulaClosure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.captures.is_empty() {
            write!(f, "{}", self.definition)
        } else {
            write!(f, "|{}| {}",
                self.captures
                    .iter()
                    .map(|(key, value)| format!("{key}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.definition
            )
        }
    }
}

/// Callable formula value backed by either a lexical closure or a registered built-in.
#[derive(Clone, Debug)]
pub enum Function {
    User(FormulaClosure),
    Builtin(Arc<str>, fn(&[EvalValue], &mut FormulaEngine, &Env) -> Result<EvalValue>),
}
impl std::fmt::Display for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User(closure) => write!(f, "<{}>", closure),
            Self::Builtin(name, _) => write!(f, "<builtin {}>", name),
        }
    }
}

impl PartialEq for Function {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::User(left), Self::User(right)) => left == right,
            (Self::Builtin(keyl, _left), Self::Builtin(keyr, _right)) => keyl == keyr,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FunctionSerializer {
    User(FormulaClosureSerializer),
    Builtin(String),
}
impl From<&Function> for FunctionSerializer {
    fn from(function: &Function) -> Self {
        match function {
            Function::User(closure) => Self::User(FormulaClosureSerializer::from(closure)),
            Function::Builtin(name, _) => Self::Builtin(name.to_string()),
        }
    }
}
impl TryFrom<FunctionSerializer> for Function {
    type Error = crate::err::Error;
    fn try_from(serializer: FunctionSerializer) -> Result<Self, Self::Error> {
        match serializer {
            FunctionSerializer::User(closure) => Ok(Self::User(closure.try_into()?)),
            FunctionSerializer::Builtin(name) => {
                let name: Arc<str> = name.into();
                let function = builtins::ALL_BUILTINS
                    .get(name.as_ref())
                    .ok_or_else(|| Error::InvalidBuiltin {
                        name: name.to_string(), reason: "Builtin function not found".into(),
                    })?;
                let EvalValue::Func(func) = function else {
                    return Err(Error::InvalidBuiltin {
                        name: name.to_string(), reason: "Builtin function was not a function".into(),
                    });
                };
                let Function::Builtin(name, ptr) = func.as_ref() else {
                    return Err(Error::InvalidBuiltin {
                        name: name.to_string(), reason: "Builtin function was not a builtin".into(),
                    });
                };
                Ok(Self::Builtin(name.clone(), *ptr))
            }
        }
    }
}

/// Parses, formats, and evaluates formulas with deterministic random support.
pub struct FormulaEngine {
    rng: ChaCha8Rng,
    cache: HashMap<Arc<str>, Expr>,
    globals: Env<'static>,
    call_stack: Vec<Arc<str>>,
    max_rec: usize,
}

impl FormulaEngine {
    /// Creates a formula engine with deterministic randomness when `seed` is supplied.
    ///
    /// Parsed expressions are cached by normalized source text for reuse.
    pub fn new(seed: Option<u64>) -> Self {
        let rng = match seed {
            Some(seed) => ChaCha8Rng::seed_from_u64(seed),
            None => ChaCha8Rng::from_rng(&mut rand::rng()),
        };
        Self {
            rng,
            cache: HashMap::new(),
            globals: GLOBAL_ENV.clone().into_engine_globals(),
            call_stack: Vec::new(),
            max_rec: 32,
        }
    }

    /// Replaces engine globals with a snapshot of every visible binding in `globals`.
    /// These bindings remain dynamic in closures unless a local binding shadows them.
    pub fn with_globals(
        self,
        globals: Env<'static>,
    ) -> Self {
        Self {
            globals: globals.into_engine_globals(),
            ..self
        }
    }
    /// Sets the maximum active call depth shared by built-in and user-defined functions.
    /// The default is 32.
    pub fn with_max_rec(mut self, max_rec: usize) -> Self {
        self.max_rec = max_rec;
        self
    }
    /// Returns the maximum active call depth shared by built-in and user-defined functions.
    pub fn get_max_rec(&self) -> usize {
        self.max_rec
    }

    /// Bounds nested function calls and restores the stack after success or an engine error.
    fn with_call_frame(
        &mut self,
        name: Arc<str>,
        operation: impl FnOnce(&mut Self) -> Result<EvalValue>,
    ) -> Result<EvalValue> {
        if self.call_stack.len() >= self.max_rec {
            let mut chain = self.call_stack.clone();
            chain.push(name);
            return Err(Error::RecursionLimit {
                limit: self.max_rec,
                call_chain: chain.join(" -> "),
            });
        }

        self.call_stack.push(name);
        // Do not propagate with `?` here: failed calls must also release their frame.
        let result = operation(self);
        self.call_stack.pop();
        result
    }
    /// Returns a pseudorandom floating-point percentage in the half-open range `0.0..100.0`.
    pub fn random_percent(&mut self) -> f64 {
        self.rng.random_range(0.0..=100.0)
    }
    /// Returns a pseudorandom d20 result in the inclusive range `1..=20`.
    pub fn d20(&mut self) -> i64 {
        self.rng.random_range(1..=20)
    }
    /// Parses (or retrieves the cached AST for) `expression` and evaluates it against `env`.
    ///
    /// An optional outer `${...}` wrapper is stripped before parsing.
    ///
    /// # Errors
    ///
    /// Returns contextual tokenizer, parser, lookup, type, arithmetic, or evaluation errors.
    pub fn evaluate(&mut self, expression: &str, env: &Env) -> Result<EvalValue> {
        let expression = full_formula(expression).unwrap_or(expression);
        let expression: Arc<str> = Arc::from(strip_formula_dollars(expression));
        let ast = if let Some(ast) = self.cache.get(&expression) {
            ast.clone()
        } else {
            let tokens = Tokenizer::new(&expression)
                .tokenize()
                .with_context(|| format!("Lexical error in formula '{expression}'"))?;
            let ast = Parser::new(tokens).parse().with_context(|| format!("Syntax error in formula '{expression}'"))?;
            self.cache.insert(expression.clone(), ast.clone());
            ast
        };
        
        let globals = self.globals.clone();
        let env = env.clone().with_parent(&globals);
        return self
            .evaluate_ast(&ast, &env)
            .with_context(|| format!("Error in formula '{expression}'"));
    }
    /// Resolves one [`EvalValue`] against `env`.
    ///
    /// A string consisting entirely of `${...}` preserves the evaluated type; other strings are formatted as text. Non-string values are cloned unchanged. Formula-looking strings produced by evaluation are not recursively resolved.
    ///
    /// # Errors
    ///
    /// Propagates formatting or formula evaluation errors.
    pub fn resolve(&mut self, value: &EvalValue, env: &Env) -> Result<EvalValue> {
        match value {
            EvalValue::String(text) => {
                if let Some(expression) = full_formula(text) {
                    self.evaluate(expression, env)
                } else {
                    Ok(EvalValue::String(self.format(text.clone(), env)?))
                }
            }
            _ => Ok(value.clone()),
        }
    }
    /// Resolves `value` once and converts the result to an integer.
    ///
    /// # Errors
    ///
    /// Returns an error when formula evaluation fails or the resolved value is not integer-compatible.
    pub fn resolve_int(&mut self, value: &EvalValue, env: &Env) -> Result<i64> {
        let resolved = self
            .resolve(value, env)
            .context("Cannot resolve the value expected to be an integer")?;
        resolved
            .to_i64()
            .with_context(|| format!("Resolution produced {}", resolved.diagnostic()))
    }
    /// Resolves `value` once and converts the result to a floating-point number.
    ///
    /// # Errors
    ///
    /// Returns an error when formula evaluation fails or the resolved value is not numeric.
    pub fn resolve_float(&mut self, value: &EvalValue, env: &Env) -> Result<f64> {
        let resolved = self
            .resolve(value, env)
            .context("Cannot resolve the value expected to be a number")?;
        resolved
            .to_f64()
            .with_context(|| format!("Resolution produced {}", resolved.diagnostic()))
    }
    /// Resolves `value` once and returns its formula-language truthiness.
    ///
    /// # Errors
    ///
    /// Propagates formula evaluation errors.
    pub fn resolve_bool(&mut self, value: &EvalValue, env: &Env) -> Result<bool> {
        let resolved = self.resolve(value, env)?;
        Ok(resolved.truthy())
    }
    /// Expands `${...}` expressions and `$variable` substitutions embedded in display text.
    ///
    /// Each discovered expression is evaluated once; formula-looking text produced by an expression is not recursively expanded during the same call.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed substitutions or formula evaluation failures.
    pub fn format(&mut self, text: Arc<str>, env: &Env) -> Result<Arc<str>> {
        let chars: Vec<char> = text.chars().collect();
        let mut output = String::new();
        let mut index = 0;

        let globals = self.globals.clone();
        let env = &env.clone().with_parent(&globals);

        while index < chars.len() {
            if chars[index] != '$' {
                output.push(chars[index]);
                index += 1;
                continue;
            }

            if chars.get(index + 1) == Some(&'$') {
                output.push('$');
                index += 2;
                continue;
            }

            if chars.get(index + 1) == Some(&'{') {
                let Some(end) = find_formula_end(&chars, index + 2) else {
                    output.push('$');
                    index += 1;
                    continue;
                };
                let expression: String = chars[index + 2..end].iter().collect();
                let value = self.evaluate(&expression, env)?;
                output.push_str(&value.repr());
                index = end + 1;
                continue;
            }

            let mut end = index + 1;
            while end < chars.len() && is_word(chars[end]) {
                end += 1;
            }
            if end == index + 1 {
                output.push('$');
                index += 1;
                continue;
            }

            let name: String = chars[index + 1..end].iter().collect();
            if let Some(value) = env.get(&name) {
                output.push_str(&value.repr());
            } else {
                output.push('$');
                output.push_str(&name);
            }
            index = end;
        }

        if output == text.as_ref() {
            return Ok(text);
        }
        Ok(Arc::from(output))
    }
    pub fn fmt_value(&mut self, value: &EvalValue, env: &Env) -> Result<Arc<str>> {
        Ok(self.resolve_formula(value, env)?.repr())
    }
    /// Resolves `value` once and evaluates it if it is a formula function.
    pub fn resolve_formula(&mut self, value: &EvalValue, env: &Env) -> Result<EvalValue> {
        match value {
            EvalValue::Func(_) => {
                let globals = self.globals.clone();
                let env = env.clone().with_parent(&globals);
                value.clone().call(&[], self, &env)
            },
            _ => Ok(value.clone()),
        }
    }
    /// Resolves `value` once and evaluates it if it is a formula function.
    pub fn resolve_formula_int(&mut self, value: &EvalValue, env: &Env) -> Result<i64> {
        let resolved = self
            .resolve_formula(value, env)
            .context("Cannot evaluate the formula expected to produce an integer")?;
        resolved
            .to_i64()
            .with_context(|| format!("The formula produced {}", resolved.diagnostic()))
    }
    pub fn resolve_formula_float(&mut self, value: &EvalValue, env: &Env) -> Result<f64> {
        let resolved = self
            .resolve_formula(value, env)
            .context("Cannot evaluate the formula expected to produce a number")?;
        resolved
            .to_f64()
            .with_context(|| format!("The formula produced {}", resolved.diagnostic()))
    }
    pub fn resolve_formula_bool(&mut self, value: &EvalValue, env: &Env) -> Result<bool> {
        Ok(self.resolve_formula(value, env)?.truthy())
    }

    /// Creates a lexical closure by copying only the free values used by its body.
    ///
    /// Engine globals stay dynamic. Local bindings are captured even when their
    /// values equal a global; capture depends on the binding's origin, never value equality.
    fn close_function(&self, function: &FormulaFunction, env: &Env) -> FormulaClosure {
        let mut captures = IndexMap::new();
        for name in function.free_variables() {
            let Some((value, origin)) = env.get_binding(name.as_ref()) else {
                // An unresolved free name may be supplied dynamically by the call environment.
                continue;
            };
            match origin {
                BindingOrigin::EngineGlobal => {}
                BindingOrigin::Local => {
                    captures.insert(name, value.clone());
                }
            }
        }
        FormulaClosure {
            definition: function.clone(),
            captures: Arc::new(captures),
        }
    }

    /// Evaluates one parsed expression against the supplied environment.
    fn evaluate_ast(&mut self, expression: &Expr, env: &Env) -> Result<EvalValue> {
        match expression {
            Expr::Literal(value) => Ok(value.clone()),
            Expr::Variable(name) => env
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    Error::UnknownVariable {
                        name: name.to_string(), available: env.visible_names_for_error(),
                    }
                }),
            Expr::Let { bindings, body } => {
                let mut local_env = env.clone();
                for (name, value) in bindings.iter() {
                    let value = self
                        .evaluate_ast(value, &local_env)
                        .with_context(|| format!("Evaluating binding `let {name}`"))?;
                    name.bind(value, &mut local_env)
                        .with_context(|| format!("Destructuring `let {name}`"))?;
                }
                self.evaluate_ast(body, &local_env)
                    .context("Evaluating the let body")
            }
            Expr::Array(values) => values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    self.evaluate_ast(value, env)
                        .with_context(|| format!("Evaluating list element {index}"))
                })
                .collect::<Result<Vec<_>>>()
                .map(|arr| EvalValue::Array(arr.into())),
            Expr::Object(entries) => {
                let mut values = IndexMap::new();
                for (index, (key, value)) in entries.iter().enumerate() {
                    let key = self
                        .evaluate_ast(key, env)
                        .with_context(|| format!("Evaluating dictionary key {index}"))?;
                    let key = match key {
                        EvalValue::String(key) => key,
                        other => bail!(Type,
                            "Dictionary key {index} is {}: a key must be a string",
                            other.diagnostic()
                        ),
                    };
                    let resolved = self
                        .evaluate_ast(value, env)
                        .with_context(|| format!("Evaluating the value associated with key `{key}`"))?;
                    values.insert(key, resolved);
                }
                Ok(EvalValue::Object(Arc::from(values)))
            }
            Expr::ListComprehension {
                value,
                comprehension,
            } => {
                let bindings = self.comprehension_bindings(comprehension, env)?;
                let mut values = Vec::new();
                for local_env in bindings {
                    if let Some(filter) = &comprehension.filter {
                        if !self.evaluate_ast(filter, &local_env)?.truthy() {
                            continue;
                        }
                    }
                    values.push(self.evaluate_ast(value, &local_env)?);
                }
                Ok(EvalValue::Array(Arc::from(values)))
            }
            Expr::ObjectComprehension {
                key,
                value,
                comprehension,
            } => {
                let bindings = self.comprehension_bindings(comprehension, env)?;
                let mut values = IndexMap::new();
                for local_env in bindings {
                    if let Some(filter) = &comprehension.filter {
                        if !self.evaluate_ast(filter, &local_env)?.truthy() {
                            continue;
                        }
                    }
                    let resolved_key = match self.evaluate_ast(key, &local_env)? {
                        EvalValue::String(key) => key,
                        other => bail!(Type,
                            "The comprehension produced key {}: a dictionary key must be a string",
                            other.diagnostic()
                        ),
                    };
                    values.insert(resolved_key, self.evaluate_ast(value, &local_env)?);
                }
                Ok(EvalValue::Object(Arc::from(values)))
            }
            Expr::Unary(operator, value) => {
                let value = self.evaluate_ast(value, env)?;
                match operator {
                    UnaryOperator::Positive => match value {
                        EvalValue::Int(_) | EvalValue::Float(_) => Ok(value),
                        other => bail!(Type,
                            "Cannot apply unary operator + to {}: a number is required",
                            other.diagnostic()
                        ),
                    },
                    UnaryOperator::Negative => match value {
                        EvalValue::Int(value) => Ok(EvalValue::Int(value.checked_neg().ok_or_else(|| Error::Overflow("Overflow in integer negation".into()))?)),
                        EvalValue::Float(value) => Ok(EvalValue::Float(-value)),
                        other => bail!(Type,
                            "Cannot apply unary operator - to {}: a number is required",
                            other.diagnostic()
                        ),
                    },
                    UnaryOperator::Not => Ok(EvalValue::Bool(!value.truthy())),
                    UnaryOperator::BitwiseNot => match value {
                        EvalValue::Int(value) => Ok(EvalValue::Int(!value)),
                        EvalValue::Bool(value) => Ok(EvalValue::Int(!(value as i64))),
                        other => bail!(Type,
                            "Cannot apply unary operator ~ to {}: an integer is required",
                            other.diagnostic()
                        ),
                    },
                }
            }
            Expr::Binary(left, BinaryOperator::And, right) => {
                let left = self.evaluate_ast(left, env)?;
                if left.truthy() {
                    self.evaluate_ast(right, env)
                } else {
                    Ok(left)
                }
            }
            Expr::Binary(left, BinaryOperator::Or, right) => {
                let left = self.evaluate_ast(left, env)?;
                if left.truthy() {
                    Ok(left)
                } else {
                    self.evaluate_ast(right, env)
                }
            }
            Expr::Binary(left, BinaryOperator::Coalesce, right) => {
                let left = self.evaluate_ast(left, env)?;
                if matches!(left, EvalValue::Null) {
                    self.evaluate_ast(right, env)
                } else {
                    Ok(left)
                }
            }
            Expr::Binary(left, operator, right) => {
                let left = self.evaluate_ast(left, env)?;
                let right = self.evaluate_ast(right, env)?;
                evaluate_binary(operator, left, right)
            }
            Expr::CompareChain(first, comparisons) => {
                let mut left = self.evaluate_ast(first, env)?;
                for (operator, right_expression) in comparisons.iter() {
                    let right = self.evaluate_ast(right_expression, env)?;
                    let result = evaluate_binary(operator, left.clone(), right.clone())?;
                    if !result.truthy() {
                        return Ok(EvalValue::Bool(false));
                    }
                    left = right;
                }
                Ok(EvalValue::Bool(true))
            }
            Expr::Ternary {
                then_expression,
                condition,
                else_expression,
            } => {
                if self.evaluate_ast(condition, env)?.truthy() {
                    self.evaluate_ast(then_expression, env)
                } else {
                    self.evaluate_ast(else_expression, env)
                }
            }
            Expr::Function(function) => {
                Ok(EvalValue::Func(Arc::new(Function::User(
                    self.close_function(function, env),
                ))))
            }
            Expr::Call(callee, arguments) => {
                let arguments = arguments
                    .iter()
                    .enumerate()
                    .map(|(index, argument)| {
                        self.evaluate_ast(argument, env).with_context(|| {
                            format!(
                                "Evaluating argument {} of call `{}`",
                                index + 1,
                                diagnostic_expression(callee)
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Postfix member calls distinguish stored function values from registered
                // methods. Only the latter receive the object as their leading argument.
                if let Expr::Get(object, member) = callee.as_ref() {
                    let object = self
                        .evaluate_ast(object, env)
                        .with_context(|| {
                            format!(
                                "Resolving the receiver of `{}`",
                                diagnostic_expression(callee)
                            )
                        })?;
                    let idx = match member {
                        Index::Int(i) => EvalValue::Int(*i),
                        Index::String(s) => EvalValue::String(s.clone()),
                    };
                    return self.call_member(
                        object,
                        idx,
                        &arguments,
                        env,
                    )
                    .with_context(|| format!("Call `{}`", diagnostic_expression(callee)));
                }
                if let Expr::Index(object, member) = callee.as_ref() {
                    let object = self
                        .evaluate_ast(object, env)
                        .with_context(|| {
                            format!(
                                "Resolving the receiver of `{}`",
                                diagnostic_expression(callee)
                            )
                        })?;
                    let member = self
                        .evaluate_ast(member, env)
                        .with_context(|| {
                            format!(
                                "Resolving the member of `{}`",
                                diagnostic_expression(callee)
                            )
                        })?;
                    return self
                        .call_member(object, member, &arguments, env)
                        .with_context(|| format!("Call `{}`", diagnostic_expression(callee)));
                }

                self.evaluate_ast(callee, env)
                    .with_context(|| {
                        format!(
                            "Resolving function `{}`",
                            diagnostic_expression(callee)
                        )
                    })?
                    .call(&arguments, self, env)
                    .with_context(|| format!("Call `{}`", diagnostic_expression(callee)))
            }
            Expr::Get(object, property) => {
                let resolved_object = self.evaluate_ast(object, env)?;
                let idx = match property {
                    Index::Int(i) => EvalValue::Int(*i),
                    Index::String(s) => EvalValue::String(s.clone()),
                };
                resolved_object
                    .get_member(&idx)
                    .with_context(|| {
                        format!(
                            "Accessing property `{property}` of {}",
                            resolved_object.diagnostic()
                        )
                    })
            }
            Expr::Index(object, index) => {
                let resolved_object = self.evaluate_ast(object, env)?;
                let resolved_index = self.evaluate_ast(index, env)?;
                resolved_object
                    .get_member(&resolved_index)
                    .with_context(|| {
                        format!(
                            "Indexing {} with {}",
                            resolved_object.diagnostic(),
                            resolved_index.diagnostic()
                        )
                    })
            }
            Expr::Slice { object, start, end, step } => {
                let object = self.evaluate_ast(object, env)?;
                let mut bound = |expr: &Option<Arc<Expr>>| -> Result<Option<i64>> {
                    match expr {
                        None => Ok(None),
                        Some(expr) => match self.evaluate_ast(expr, env)? {
                            EvalValue::Null => Ok(None),
                            EvalValue::Int(value) => Ok(Some(value)),
                            other => bail!(Type, "A slice bound or step must be an integer or null, got {}", other.diagnostic()),
                        },
                    }
                };
                let start = bound(start)?;
                let end = bound(end)?;
                let step = bound(step)?.unwrap_or(1);
                slice_value(&object, start, end, step)
            }
        }
    }

    /// Evaluates an expression in the given environment, using the engine's globals as the parent environment.
    pub fn evalexpr(&mut self, expression: &Expr, env: &Env) -> Result<EvalValue> {
        let globals = self.globals.clone();
        self.evaluate_ast(expression, &env.clone().with_parent(&globals))
    }

    /// Calls a stored member or registered method with the correct receiver semantics.
    fn call_member(
        &mut self,
        object: EvalValue,
        member: EvalValue,
        arguments: &[EvalValue],
        env: &Env,
    ) -> Result<EvalValue> {
        if let Some(function) = object.direct_member(&member)? {
            return function.call(arguments, self, env);
        }

        let function = object.get_member(&member)?;
        let mut method_arguments = Vec::with_capacity(arguments.len() + 1);
        method_arguments.push(object);
        method_arguments.extend_from_slice(arguments);
        function.call(&method_arguments, self, env)
    }
    
    /// Evaluates one user-defined function in its captured lexical environment.
    ///
    /// Parameters and `args` have highest priority, followed by captured lexical values,
    /// engine globals, and unresolved caller bindings. The caller in [`EvalValue::call`]
    /// enforces the shared function-call depth limit.
    fn call_user_function(
        &mut self,
        closure: FormulaClosure,
        arguments: &[EvalValue],
        env: &Env,
        name: &str,
    ) -> Result<EvalValue> {
        let FormulaClosure {
            definition: function,
            captures,
        } = closure;
        if let Some(parameters) = &function.parameters {
            if arguments.len() < parameters.len() {
                return Err(Error::Arity {
                    function: name.to_string(),
                    expected: ArgumentCount::AtLeast(parameters.len()),
                    actual: arguments.len(),
                });
            }
        }

        let captured_env = Env::from_arc(captures);
        // Globals are placed before the remaining caller environment. A local value that
        // shadowed one of them at closure creation was captured and therefore still wins.
        let globals = self.globals.clone();
        let mut local_env = Env::new()
            .with_parent(&captured_env)
            .with_parent(&globals)
            .with_parent(env);
        local_env.insert("args", EvalValue::Array(arguments.to_vec().into()));
        if let Some(parameters) = &function.parameters {
            for (parameter, value) in parameters.iter().zip(arguments.iter()) {
                parameter.bind(value.clone(), &mut local_env)
                    .with_context(|| format!("Parameter {parameter} of {name}"))?;
            }
        }

        self
            .evaluate_ast(&function.body, &local_env)
            .with_context(|| {
                format!(
                    "In {name}, body `{}`",
                    diagnostic_expression(&function.body)
                )
            })
    }
    
    /// Expands one comprehension source into local environments with bound loop variables.
    fn comprehension_bindings<'a>(
        &mut self,
        comprehension: &Comprehension,
        env: &Env<'a>,
    ) -> Result<Vec<Env<'a>>> {
        let iterable = self.evaluate_ast(&comprehension.iterable, env)?;
        let rows = comprehension_rows(iterable, comprehension.targets.len())?;
        let pattern = if comprehension.targets.len() == 1 {
            comprehension.targets[0].clone()
        } else {
            BindingPattern::List(comprehension.targets.to_vec())
        };
        rows.into_iter()
            .map(|row| {
                let mut local_env = env.clone();
                let value = if comprehension.targets.len() == 1 {
                    row[0].clone()
                } else {
                    EvalValue::Array(row.clone())
                };
                pattern.bind(value, &mut local_env)
                    .with_context(|| format!("Destructuring comprehension with {pattern}"))?;
                Ok(local_env)
            })
            .collect()
    }
}

/// Validates the number of arguments supplied to a formula function or method.
fn expect_arity(name: &str, arguments: &[EvalValue], expected: usize) -> Result<()> {
    if arguments.len() != expected {
        return Err(Error::Arity {
            function: name.to_owned(), expected: ArgumentCount::Exactly(expected),
            actual: arguments.len(),
        });
    }
    Ok(())
}

fn expect_arity_between(name: &str, actual: usize, min: usize, max: usize) -> Result<()> {
    if !(min..=max).contains(&actual) {
        return Err(Error::Arity {
            function: name.to_owned(), expected: ArgumentCount::Between { min, max }, actual,
        });
    }
    Ok(())
}

/// Raises one numeric value to another numeric power.
fn power_values(base: &EvalValue, exponent: &EvalValue) -> Result<EvalValue> {
    if let (EvalValue::Int(base), EvalValue::Int(exponent)) = (base, exponent) {
        if *exponent >= 0 {
            let exponent = u32::try_from(*exponent)
                .map_err(|_| error!(Overflow, "The exponent for pow is too large"))?;
            return base
                .checked_pow(exponent)
                .map(EvalValue::Int)
                .ok_or_else(|| error!(Overflow, "Numeric overflow in pow"));
        }
    }

    let base = base.to_f64()?;
    let exponent = exponent.to_f64()?;
    if base == 0.0 && exponent < 0.0 {
        bail!(NumericDomain, "pow cannot raise zero to a negative power")
    }
    if base < 0.0 && exponent.fract() != 0.0 {
        bail!(NumericDomain, "pow does not support complex results")
    }
    let result = base.powf(exponent);
    Ok(EvalValue::Float(result))
}

/// Sums numeric list values while preserving an integral result when possible.
fn sum_values(values: &[EvalValue]) -> Result<EvalValue> {
    let mut integer_total = 0_i64;
    let mut float_total = 0.0_f64;
    let mut has_float = false;
    for value in values {
        match value {
            EvalValue::Int(value) => {
                if has_float {
                    float_total += *value as f64;
                } else {
                    integer_total = integer_total
                        .checked_add(*value)
                        .ok_or_else(|| error!(Overflow, "Numeric overflow in sum"))?;
                }
            }
            EvalValue::Bool(value) => {
                let value = if *value { 1_i64 } else { 0_i64 };
                if has_float {
                    float_total += value as f64;
                } else {
                    integer_total = integer_total
                        .checked_add(value)
                        .ok_or_else(|| error!(Overflow, "Numeric overflow in sum"))?;
                }
            }
            EvalValue::Float(value) => {
                if !has_float {
                    float_total = integer_total as f64;
                    has_float = true;
                }
                float_total += value;
            }
            value => bail!(Type,
                "sum cannot add {}: every list element must be numeric",
                value.diagnostic()
            ),
        }
    }
    if has_float {
        Ok(EvalValue::Float(float_total))
    } else {
        Ok(EvalValue::Int(integer_total))
    }
}

/// Multiplies numeric list values while preserving an integral result when possible.
fn mul_values(values: &[EvalValue]) -> Result<EvalValue> {
    let mut integer_total = 1_i64;
    let mut float_total = 1.0_f64;
    let mut has_float = false;
    for value in values {
        match value {
            EvalValue::Int(value) => {
                if has_float {
                    float_total *= *value as f64;
                } else {
                    integer_total = integer_total
                        .checked_mul(*value)
                        .ok_or_else(|| error!(Overflow, "Numeric overflow in mul"))?;
                }
            }
            EvalValue::Bool(value) => {
                let value = if *value { 1_i64 } else { 0_i64 };
                if has_float {
                    float_total *= value as f64;
                } else {
                    integer_total = integer_total
                        .checked_mul(value)
                        .ok_or_else(|| error!(Overflow, "Numeric overflow in mul"))?;
                }
            }
            EvalValue::Float(value) => {
                if !has_float {
                    float_total = integer_total as f64;
                    has_float = true;
                }
                float_total *= value;
            }
            value => bail!(Type,
                "mul cannot multiply {}: every list element must be numeric",
                value.diagnostic()
            ),
        }
    }
    if has_float {
        Ok(EvalValue::Float(float_total))
    } else {
        Ok(EvalValue::Int(integer_total))
    }
}

fn indexof_str(haystack: &str, needle: &str) -> EvalValue {
    if let Some(index) = haystack.find(needle) {
        let scalar_index = haystack[..index].chars().count();
        EvalValue::Int(scalar_index as i64)
    } else {
        EvalValue::Null
    }
}
fn indexof_object(haystack: &IndexMap<Arc<str>, EvalValue>, needle: &EvalValue) -> EvalValue {
    for (key, value) in haystack.iter() {
        if value == needle {
            return EvalValue::String(key.clone());
        }
    }
    EvalValue::Null
}
fn zip_arrays(arrays: &[Arc<[EvalValue]>]) -> EvalValue {
    let mut res = Vec::new();
    let mut its = arrays.iter().map(|array| array.iter()).collect::<Vec<_>>();
    loop {
        let mut row = Vec::new();
        for it in &mut its {
            if let Some(value) = it.next() {
                row.push(value.clone());
            } else {
                return EvalValue::Array(res.into());
            }
        }
        res.push(EvalValue::Array(row.into()));
    }
}

fn get_path(obj: &EvalValue, path: &[&str], default: Option<EvalValue>) -> Result<EvalValue> {
    if path.is_empty() {
        return Ok(obj.clone());
    }
    match (obj, path[0].parse::<usize>()) {
        (EvalValue::Array(values), Ok(index)) => {
            if let Some(value) = values.get(index) {
                get_path(value, &path[1..], default)
            } else {
                default.ok_or_else(|| Error::IndexOutOfBounds { index: index as i128, length: values.len() })
            }
        }
        (EvalValue::Object(values), Err(_)) => {
            if let Some(value) = values.get(path[0]) {
                get_path(value, &path[1..], default)
            } else {
                default.ok_or_else(|| Error::UnknownKey { key: path[0].to_owned() })
            }
        }
        _ => bail!(Type,
            "Cannot access segment `{}` in {}: traversal requires a list indexed by an integer or a dictionary indexed by a string",
            path[0],
            obj.diagnostic()
        ),
    }
}
fn has_path(obj: &EvalValue, path: &[&str]) -> bool {
    if path.is_empty() {
        return true;
    }
    match (obj, path[0].parse::<usize>()) {
        (EvalValue::Array(values), Ok(index)) => values
            .get(index)
            .map_or(false, |value| has_path(value, &path[1..])),
        (EvalValue::Object(values), Err(_)) => values
            .get(path[0])
            .map_or(false, |value| has_path(value, &path[1..])),
        _ => false,
    }
}
fn shuffle(values: &mut [EvalValue], rng: &mut impl Rng) {
    for i in (1..values.len()).rev() {
        let j = rng.random_range(0..=i);
        values.swap(i, j);
    }
}
fn minmax(values: &[EvalValue], ord: Ordering) -> Result<EvalValue> {
    let mut iter = values.iter();
    let first = iter
        .next()
        .ok_or_else(|| error!(InvalidArgument, "min/max requires at least one argument"))?;
    let mut best = first.clone();
    for value in iter {
        if value.partial_cmp(&best) == Some(ord) {
            best = value.clone();
        }
    }
    Ok(best)
}

/// Parses a formula expression without evaluating it.
///
/// An optional outer `${...}` wrapper is accepted. This is used while loading declared
/// functions and by authoring tools that need syntax validation without a runtime environment.
///
/// # Errors
///
/// Returns tokenizer or parser errors for malformed input.
pub fn validate_formula_syntax(expression: &str) -> Result<()> {
    let expression = full_formula(expression).unwrap_or(expression);
    let expression = strip_formula_dollars(expression);
    let tokens = Tokenizer::new(&expression)
        .tokenize()
        .with_context(|| format!("Lexical error in formula '{expression}'"))?;
    Parser::new(tokens).parse().with_context(|| format!("Syntax error in formula '{expression}'"))?;
    Ok(())
}

/// Parses the next expression, stopping when the grammar cannot extend it.
///
/// Returns the expression and the zero-based byte offset immediately after
/// its last token. 
/// The offset counts UTF-8 bytes, not Unicode scalar values.
/// Text following the expression does not need to be valid formula syntax.
///
/// # Errors
///
/// Returns an error for an empty or invalid expression, including an incomplete
/// continuation such as `a +` or `a(`.
///
/// # Examples
///
/// ```
/// let (_, end) = formulas::parse_next("a*a+3, b+5")?;
/// assert_eq!(end, 5);
/// # Ok::<(), formulas::Error>(())
/// ```
pub fn parse_next(source: &str) -> Result<(Expr, usize)> {
    let mut parser = Parser::new(Vec::new());
    parser.tokenizer = Some(Tokenizer::new(source));
    let expression = parser
        .parse_expression()
        .map_err(|error| parser.lexical_error.take().unwrap_or(error))?;
    let char_end = parser.token_ends[parser.position - 1];
    let byte_end = source.char_indices().nth(char_end).map(|(i, _)| i).unwrap_or(source.len());
    Ok((expression, byte_end))
}

/// Returns whether a text contains at least one unescaped formula substitution.
pub fn has_formula_substitution(text: &str) -> bool {
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' {
            index += 1;
            continue;
        }
        if chars.get(index + 1) == Some(&'$') {
            index += 2;
            continue;
        }
        if chars.get(index + 1) == Some(&'{') {
            return true;
        }
        if chars.get(index + 1).copied().map(is_word).unwrap_or(false) {
            return true;
        }
        index += 1;
    }
    false
}

/// Extracts the expression from a string that consists entirely of one formula.
/// 
/// Leading and trailing whitespace is allowed outside the formula, but only if it is not wrapped in `${...}`, in which case the delimiters must be at the very start and end of the string.
///
pub fn full_formula(text: &str) -> Option<&str> {
    if !text.starts_with("${") {
        let text = text.trim();
        let (_next_expr, end) = parse_next(text).ok()?;
        if end != text.len() {
            return None;
        }
        return Some(&text[..end]);
    }
    let chars = text.chars().collect::<Vec<_>>();
    let end = find_formula_end(&chars, 2)?;
    if end + 1 != chars.len() || end == 2 {
        return None;
    }
    Some(&text[2..text.len() - 1])
}

/// Finds the closing quote of a string whose entire contents are one `${...}` formula.
///
/// Embedded formulas can use the same quote character for the outer
/// formula-language string and for literals inside the embedded formula, such
/// as `'${status == 'ready'}'`. The `${...}` delimiters make the intended outer
/// boundary unambiguous, so the tokenizer can preserve the complete formula
/// string instead of stopping at its first inner quote.
fn quoted_formula_literal_end(chars: &[char], quote_start: usize) -> Option<usize> {
    let quote = *chars.get(quote_start)?;
    if !matches!(quote, '\'' | '"')
        || chars.get(quote_start + 1) != Some(&'$')
        || chars.get(quote_start + 2) != Some(&'{')
    {
        return None;
    }

    let formula_end = find_formula_end(chars, quote_start + 3)?;
    (chars.get(formula_end + 1) == Some(&quote)).then_some(formula_end + 1)
}

/// Finds the closing delimiter of an inline formula while ignoring dictionary braces and strings.
fn find_formula_end(chars: &[char], start: usize) -> Option<usize> {
    let mut brace_depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in chars.iter().copied().enumerate().skip(start) {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '{' => brace_depth += 1,
            '}' if brace_depth == 0 => return Some(index),
            '}' => brace_depth -= 1,
            _ => {}
        }
    }
    None
}

/// Converts an iterable value into rows suitable for comprehension variable binding.
fn comprehension_rows(iterable: EvalValue, target_count: usize) -> Result<Arc<[Arc<[EvalValue]>]>> {
    match (iterable, target_count) {
        (EvalValue::Array(values), 1) => Ok(values.into_iter().map(|value| vec![value.clone()].into()).collect::<Arc<[Arc<[EvalValue]>]>>()),
        (EvalValue::Array(values), _) => values
            .iter()
            .map(|value| match value {
                EvalValue::Array(row) => Ok(row.clone()),
                _ => bail!(Type, "List unpacking requires nested lists"),
            })
            .collect(),
        (EvalValue::Object(values), 1) => Ok(values
            .keys()
            .map(|key| vec![EvalValue::String(key.clone())].into())
            .collect::<Arc<[Arc<[EvalValue]>]>>()),
        (EvalValue::Object(values), 2) => Ok(values
            .iter()
            .map(|(key, value)| vec![EvalValue::String(key.clone()), value.clone()].into())
            .collect::<Arc<[Arc<[EvalValue]>]>>()),
        (EvalValue::Object(_), expected) => bail!(InvalidArgument,
            "A dictionary can be iterated with one variable (key) or two variables (key, value), not {expected}"
        ),
        (EvalValue::String(value), 1) => Ok(value
            .chars()
            .map(|character| vec![EvalValue::String(Arc::from(character.to_string()))].into())
            .collect::<Arc<[Arc<[EvalValue]>]>>()),
        (EvalValue::String(_), _) => {
            bail!(InvalidArgument, "A string can only be iterated with one variable")
        }
        (value, _) => bail!(Type,
            "A comprehension cannot iterate over {}: a list, dictionary, or string is required",
            value.diagnostic()
        ),
    }
}

/// Rounds a number using Python-compatible halfway-to-even semantics.
fn python_round(value: f64, digits: i64) -> Result<f64> {
    let digits = i32::try_from(digits).map_err(|_| error!(InvalidArgument, "Number of decimal places out of range"))?;
    let factor = 10_f64.powi(digits);
    
    if !factor.is_finite() || factor == 0.0 {
        bail!(InvalidArgument, "Number of decimal places out of range")
    }
    let scaled = value * factor;
    if !scaled.is_finite() {
        bail!(NumericDomain, "Cannot round this value")
    }
    // let floor = scaled.floor();
    // let fraction = scaled - floor;
    // let tolerance = f64::EPSILON * scaled.abs().max(1.0) * 4.0;
    let rounded = scaled.round_ties_even();
    Ok(rounded / factor)
}

/// Removes optional formula sigils from a variable reference.
fn strip_formula_dollars(expression: &str) -> String {
    let chars: Vec<char> = expression.chars().collect();
    let mut output = String::new();
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < chars.len() {
        let character = chars[index];
        if let Some(active_quote) = quote {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if let Some(quote_end) = quoted_formula_literal_end(&chars, index) {
                output.extend(chars[index..=quote_end].iter());
                index = quote_end + 1;
                continue;
            }
            quote = Some(character);
            output.push(character);
            index += 1;
            continue;
        }
        if character == '$'
            && chars
                .get(index + 1)
                .is_some_and(|character| is_word(*character))
        {
            index += 1;
            continue;
        }
        output.push(character);
        index += 1;
    }
    output
}

/// Returns whether a character may participate in an identifier.
fn is_word(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

/// Extracts a list or Unicode-character string slice with clamped, exclusive bounds.
fn slice_value(
    value: &EvalValue,
    start: Option<i64>,
    end: Option<i64>,
    step: i64,
) -> Result<EvalValue> {
    if step == 0 {
        bail!(InvalidArgument, "A slice step cannot be zero");
    }
    let characters;
    let length = match value {
        EvalValue::Array(values) => {
            characters = Vec::new();
            values.len()
        }
        EvalValue::String(text) => {
            characters = text.chars().collect::<Vec<_>>();
            characters.len()
        }
        other => bail!(Type,
            "A slice requires a list or string, got {}",
            other.diagnostic()
        ),
    } as i128;
    let reverse = step < 0;
    // i128 avoids overflow for extreme i64 bounds and steps, including i64::MIN.
    let normalize = |bound: i64| {
        let bound = bound as i128;
        let bound = if bound < 0 { length + bound } else { bound };
        if reverse {
            bound.clamp(-1, length - 1)
        } else {
            bound.clamp(0, length)
        }
    };
    let mut index = start
        .map(normalize)
        .unwrap_or(if reverse { length - 1 } else { 0 });
    let stop = end
        .map(normalize)
        .unwrap_or(if reverse { -1 } else { length });
    let mut values = Vec::new();
    let mut text = String::new();
    while if reverse { index > stop } else { index < stop } {
        match value {
            EvalValue::Array(source) => values.push(source[index as usize].clone()),
            EvalValue::String(_) => text.push(characters[index as usize]),
            _ => unreachable!(),
        }
        index += step as i128;
    }
    Ok(match value {
        EvalValue::Array(_) => EvalValue::Array(values.into()),
        _ => EvalValue::String(text.into()),
    })
}

/// Normalizes a possibly negative index against a collection length.
fn normalize_index(index: i64, length: usize) -> Result<usize> {
    let requested = index;
    let length = length as i64;
    let index = if index < 0 { length + index } else { index };
    if index < 0 || index >= length {
        return Err(Error::IndexOutOfBounds { index: requested as i128, length: length as usize });
    }
    Ok(index as usize)
}

/// Applies one binary operator to two already evaluated operands.
///
/// # Errors
///
/// Returns an error for unsupported operand types, division by zero, invalid shifts, or other
/// operator-specific domain failures.
fn evaluate_binary(
    operator: &BinaryOperator,
    left: EvalValue,
    right: EvalValue,
) -> Result<EvalValue> {
    match operator {
        BinaryOperator::Add => add_values(left, right),
        BinaryOperator::Subtract => {
            numeric_binary(left, right, |a, b| a - b, |a, b| a.checked_sub(b))
        }
        BinaryOperator::Multiply => multiply_values(left, right),
        BinaryOperator::Divide => {
            let divisor = right
                .to_f64()
                .with_context(|| format!("The divisor is {}", right.diagnostic()))?;
            if divisor == 0.0 {
                bail!(DivisionByZero,
                    "Cannot divide {} by {}: division by zero",
                    left.diagnostic(),
                    right.diagnostic()
                )
            }
            let dividend = left
                .to_f64()
                .with_context(|| format!("The dividend is {}", left.diagnostic()))?;
            Ok(EvalValue::Float(dividend / divisor))
        }
        BinaryOperator::FloorDivide => floor_divide(left, right),
        BinaryOperator::Modulo => modulo_values(left, right),
        BinaryOperator::Equal => Ok(EvalValue::Bool(values_equal(&left, &right))),
        BinaryOperator::NotEqual => Ok(EvalValue::Bool(!values_equal(&left, &right))),
        BinaryOperator::StrictEqual => Ok(EvalValue::Bool(left.strict_eq(&right))),
        BinaryOperator::StrictNotEqual => Ok(EvalValue::Bool(!left.strict_eq(&right))),
        BinaryOperator::Less => Ok(EvalValue::Bool(
            compare_values(&left, &right)? == Ordering::Less,
        )),
        BinaryOperator::LessEqual => Ok(EvalValue::Bool(matches!(
            compare_values(&left, &right)?,
            Ordering::Less | Ordering::Equal
        ))),
        BinaryOperator::Greater => Ok(EvalValue::Bool(
            compare_values(&left, &right)? == Ordering::Greater,
        )),
        BinaryOperator::GreaterEqual => Ok(EvalValue::Bool(matches!(
            compare_values(&left, &right)?,
            Ordering::Greater | Ordering::Equal
        ))),
        BinaryOperator::In => Ok(EvalValue::Bool(contains_value(&right, &left)?)),
        BinaryOperator::NotIn => Ok(EvalValue::Bool(!contains_value(&right, &left)?)),
        BinaryOperator::BitAnd => bitwise_binary(left, right, "&", |a, b| a & b, None),
        BinaryOperator::BitXor => bitwise_binary(left, right, "^", |a, b| a ^ b, None),
        BinaryOperator::BitOr => bitwise_binary(left, right, "|", |a, b| a | b, None),
        BinaryOperator::Xor => Ok(EvalValue::Bool(left.truthy() != right.truthy())),
        BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Coalesce => unreachable!("handled by short-circuit evaluation"),
        BinaryOperator::LeftShift => bitwise_binary(
            left, 
            right, 
            "<<", 
            |a, b| if b > u32::MAX as i64 { 0 } else { a.unbounded_shl(b as u32) },
            Some(&|_, b| b >= 0),
        ),
        BinaryOperator::RightShift => bitwise_binary(
            left, 
            right, 
            ">>", 
            |a, b| if b > u32::MAX as i64 { if a >= 0 { 0 } else { -1 } } else { a.unbounded_shr(b as u32) },
            Some(&|_, b| b >= 0),
        ),
        BinaryOperator::LogicRightShift => bitwise_binary(
            left,
            right,
            ">>>",
            |a, b| if b > u32::MAX as i64 { 0 } else { (a as u64).unbounded_shr(b as u32) as i64 },
            Some(&|_, b| b >= 0),
        ),
    }
}
/// Applies formula-language `+` semantics to two already evaluated operands.
///
/// # Errors
///
/// Returns an error for unsupported type pairs.
fn add_values(left: EvalValue, right: EvalValue) -> Result<EvalValue> {
    left.try_add(&right)
}
fn range_values(args: &[&EvalValue]) -> Result<EvalValue> {
    let (start, stop, step) = match args.len() {
        1 => (0_i64, args[0].to_i64()?, 1_i64),
        2 => (args[0].to_i64()?, args[1].to_i64()?, 1_i64),
        3 => (args[0].to_i64()?, args[1].to_i64()?, args[2].to_i64()?),
        _ => return Err(Error::Arity {
            function: "range".into(), expected: ArgumentCount::Between { min: 1, max: 3 }, actual: args.len(),
        }),
    };
    if step == 0 {
        bail!(InvalidArgument, "range cannot have a zero step")
    }
    let mut values = Vec::new();
    let mut current = start;
    if step > 0 {
        while current < stop {
            values.push(EvalValue::Int(current));
            let check = current.checked_add(step);
            if let Some(next) = check {
                current = next;
            } else {
                break;
            }
        }
    } else {
        while current > stop {
            values.push(EvalValue::Int(current));
            let check = current.checked_add(step);
            if let Some(next) = check {
                current = next;
            } else {
                break;
            }
        }
    }
    Ok(EvalValue::Array(values.into()))
}
fn pop_values(array: &[&EvalValue], i: Option<usize>) -> Result<EvalValue> {
    if array.is_empty() {
        bail!(InvalidArgument, "pop cannot be called on an empty list")
    }
    let index = i.unwrap_or(array.len() - 1);
    if index >= array.len() {
        return Err(Error::IndexOutOfBounds { index: index as i128, length: array.len() });
    }
    let mut values = array.iter().map(|v| (*v).clone()).collect::<Vec<_>>();
    let popped = values.remove(index);
    Ok(EvalValue::Array(
        vec![EvalValue::Array(values.into()), popped].into(),
    ))
}

/// Multiplies numeric values or repeats supported sequence values.
fn multiply_values(left: EvalValue, right: EvalValue) -> Result<EvalValue> {
    left.try_mul(&right)
}

/// Applies a numeric binary operator after coercing compatible operands.
fn numeric_binary(
    left: EvalValue,
    right: EvalValue,
    float_operation: impl FnOnce(f64, f64) -> f64,
    int_operation: impl FnOnce(i64, i64) -> Option<i64>,
) -> Result<EvalValue> {
    match (&left, &right) {
        (EvalValue::Int(left), EvalValue::Int(right)) => int_operation(*left, *right)
            .map(EvalValue::Int)
            .ok_or_else(|| error!(Overflow, "Numeric overflow")),
        _ => Ok(EvalValue::Float(float_operation(
            left.to_f64()?,
            right.to_f64()?,
        ))),
    }
}

/// Performs Python-style floor division and reports division by zero.
fn floor_divide(left: EvalValue, right: EvalValue) -> Result<EvalValue> {
    left.try_floor_div(&right)
}

/// Computes a Python-style remainder and reports invalid operands.
fn modulo_values(left: EvalValue, right: EvalValue) -> Result<EvalValue> {
    left.try_mod(&right)
}

/// Applies one bitwise operation to two integer operands.
fn bitwise_binary(
    left: EvalValue,
    right: EvalValue,
    symbol: &str,
    operation: impl FnOnce(i64, i64) -> i64,
    constraint: Option<&dyn Fn(i64, i64) -> bool>,
) -> Result<EvalValue> {
    let left = if let EvalValue::Bool(val) = left {
        EvalValue::Int(if val { 1 } else { 0 })
    } else {
        left
    };
    let right = if let EvalValue::Bool(val) = right {
        EvalValue::Int(if val { 1 } else { 0 })
    } else {
        right
    };
    let (EvalValue::Int(left), EvalValue::Int(right)) = (&left, &right) else {
        bail!(Type,
            "Cannot apply bitwise operator {symbol} to {} and {}: two integers are required",
            left.diagnostic(),
            right.diagnostic()
        )
    };
    if let Some(constraint) = constraint {
        if !constraint(*left, *right) {
            bail!(InvalidArgument,
                "Values {} and {} do not satisfy the constraint for bitwise operator {symbol}",
                left,
                right
            )
        }
    }
    Ok(EvalValue::Int(operation(*left, *right)))
}

/// Compares evaluated values using formula-language equality semantics.
fn values_equal(left: &EvalValue, right: &EvalValue) -> bool {
    left == right
}

/// Orders compatible evaluated values for comparison operators.
fn compare_values(left: &EvalValue, right: &EvalValue) -> Result<Ordering> {
    left.partial_cmp(right).ok_or_else(|| {
        error!(Type,
            "Cannot compare {} and {}: these types have no common ordering",
            left.diagnostic(),
            right.diagnostic()
        )
    })
}

/// Returns whether a value participates in Python-style numeric operations.
#[allow(dead_code)]
fn is_python_number(value: &EvalValue) -> bool {
    matches!(
        value,
        EvalValue::Bool(_) | EvalValue::Int(_) | EvalValue::Float(_)
    )
}

/// Tests membership using formula-language equality semantics.
fn contains_value(container: &EvalValue, needle: &EvalValue) -> Result<bool> {
    match container {
        EvalValue::Array(values) => Ok(values.iter().any(|value| values_equal(value, needle))),
        EvalValue::String(value) => match needle {
            EvalValue::String(needle) => Ok(value.contains(needle.as_ref())),
            _ => bail!(Type,
                "Cannot search for {} in {}: a string can only contain another string",
                needle.diagnostic(),
                container.diagnostic()
            ),
        },
        EvalValue::Object(values) => match needle {
            EvalValue::String(needle) => Ok(values.contains_key(needle)),
            _ => bail!(Type,
                "Cannot search for {} in {}: a dictionary key must be a string",
                needle.diagnostic(),
                container.diagnostic()
            ),
        },
        _ => bail!(Type,
            "Cannot search for {} in {}: the `in` operator requires a list, string, or dictionary as its container",
            needle.diagnostic(),
            container.diagnostic()
        ),
    }
}

/// Returns the current value except in the case it is a formula string, in which case it is parsed and transformed into a no argument function.
pub fn value_or_formula(
    value: EvalValue,
) -> Result<EvalValue> {
    match value {
        EvalValue::String(s) => {
            match full_formula(&s)  {
                None => Ok(EvalValue::String(s)),
                Some(inner) => {
                    let expr = Arc::from(strip_formula_dollars(inner).as_str());
                    let tokens = Tokenizer::new(&expr)
                        .tokenize()
                        .with_context(|| format!("Lexical error in formula '{expr}'"))?;
                    let ast = Parser::new(tokens).parse().with_context(|| format!("Syntax error in formula '{expr}'"))?;
                    Ok(EvalValue::Func(Arc::new(Function::User(FormulaClosure { 
                        definition: FormulaFunction { 
                            parameters: None,
                            body: Arc::from(ast),
                        }, 
                        captures: IndexMap::new().into(), 
                    }))))
                },
            }
        },
        _ => Ok(value),
    }
}

/// Split a string into a vector of alternating literal and formula segments, preserving the original text.
/// and returns a function that will evaluate the formula segments and concatenate the results with the literal segments.
/// Or returns a str if there are no formula segments.
pub fn split_fmt_str(text: &str) -> Result<EvalValue>{
    let mut segments = Vec::new();
    let chars = text.chars().collect::<Vec<_>>();
    let mut current_segment = String::new();
    let mut index = 0;

    while index < chars.len() {
        if chars[index] != '$' {
            current_segment.push(chars[index]);
            index += 1;
            continue;
        }

        if chars.get(index + 1) == Some(&'$') {
            current_segment.push('$');
            index += 2;
            continue;
        }

        if chars.get(index + 1) == Some(&'{') {
            let Some(end) = find_formula_end(&chars, index + 2) else {
                current_segment.push('$');
                index += 1;
                continue;
            };
            let expression: String = chars[index + 2..end].iter().collect();
            let expression = strip_formula_dollars(&expression);
            
            if !current_segment.is_empty() {
                segments.push(EvalValue::String(Arc::from(current_segment)));
                current_segment = String::new();
            }
            let tokens = Tokenizer::new(&expression)
                .tokenize()
                .with_context(|| format!("Lexical error in formula '{expression}'"))?;
            let ast = Parser::new(tokens).parse().with_context(|| format!("Syntax error in formula '{expression}'"))?;
            segments.push(EvalValue::Func(Arc::new(Function::User(FormulaClosure {
                definition: FormulaFunction {
                    parameters: None,
                    body: Arc::from(ast),
                },
                captures: IndexMap::new().into(),
            }))));

            index = end + 1;
            continue;
        }

        let mut end = index + 1;
        while end < chars.len() && is_word(chars[end]) {
            end += 1;
        }
        if end == index + 1 {
            current_segment.push('$');
            index += 1;
            continue;
        }

        let name: String = chars[index + 1..end].iter().collect();
        let formulastr = format!("{name}");
        let tokens = Tokenizer::new(&formulastr)
            .tokenize()
            .with_context(|| format!("Lexical error in formula '{formulastr}'"))?;
        let ast = Parser::new(tokens).parse().with_context(|| format!("Syntax error in formula '{formulastr}'"))?;
        let func = EvalValue::Func(Arc::new(Function::User(FormulaClosure {
            definition: FormulaFunction {
                parameters: None,
                body: Arc::from(ast),
            },
            captures: IndexMap::new().into(),
        })));
        if !current_segment.is_empty() {
            segments.push(EvalValue::String(Arc::from(current_segment)));
            current_segment = String::new();
        }
        segments.push(func);
        index = end;
    }

    if !current_segment.is_empty() {
        segments.push(EvalValue::String(Arc::from(current_segment)));
    }

    if segments.iter().all(|s| if let EvalValue::String(_) = s {true} else {false}) {
        let concatenated = segments.iter().map(|s| if let EvalValue::String(s) = s {s.clone()} else {Arc::from("")}).collect::<Vec<_>>().join("");
        //println!("Resolved string: {}", concatenated);
        Ok(EvalValue::String(Arc::from(concatenated)))
    } else {
        if segments.len() == 1 { // If there is only one segment, return it directly without creating a function.
            //println!("Resolved single segment: {}", segments[0]);
            return Ok(segments[0].clone());
        }
        let segments = EvalValue::Array(Arc::from(segments));
        let body = Expr::Call(
            Arc::new(Expr::Get(
                Arc::new(Expr::Literal(segments)),
                "fmt".into(),
            )),
            Arc::from([]),
        );
        let func = EvalValue::Func(Arc::new(Function::User(FormulaClosure {
            definition: FormulaFunction {
                parameters: None,
                body: Arc::from(body),
            },
            captures: IndexMap::new().into(),
        })));
        //println!("Resolved function: {}", func);
        Ok(func)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Index {
    Int(i64),
    String(Arc<str>),
}

impl std::fmt::Display for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Index::Int(i) => write!(f, "{}", i),
            Index::String(s) => write!(f, "{}", s),
        }
    }
}
impl From<&str> for Index {
    fn from(s: &str) -> Self {
        Index::String(Arc::from(s))
    }
}
impl From<String> for Index {
    fn from(s: String) -> Self {
        Index::String(Arc::from(s))
    }
}
impl From<i64> for Index {
    fn from(i: i64) -> Self {
        Index::Int(i)
    }
}

/// Abstract syntax tree node for a parsed formula expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(EvalValue),
    Variable(Arc<str>),
    /// Anonymous user-defined function created by `fn ... -> ...`.
    Function(FormulaFunction),
    Let {
        bindings: Arc<[(BindingPattern, Expr)]>,
        body: Arc<Expr>,
    },
    Array(Arc<[Expr]>),
    Object(Arc<[(Expr, Expr)]>),
    ListComprehension {
        value: Arc<Expr>,
        comprehension: Comprehension,
    },
    ObjectComprehension {
        key: Arc<Expr>,
        value: Arc<Expr>,
        comprehension: Comprehension,
    },
    Unary(UnaryOperator, Arc<Expr>),
    Binary(Arc<Expr>, BinaryOperator, Arc<Expr>),
    CompareChain(Arc<Expr>, Arc<[(BinaryOperator, Expr)]>),
    Ternary {
        then_expression: Arc<Expr>,
        condition: Arc<Expr>,
        else_expression: Arc<Expr>,
    },
    Call(Arc<Expr>, Arc<[Expr]>),
    Get(Arc<Expr>, Index),
    Index(Arc<Expr>, Arc<Expr>),
    Slice {
        object: Arc<Expr>,
        start: Option<Arc<Expr>>,
        end: Option<Arc<Expr>>,
        step: Option<Arc<Expr>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExprSerializer {
    Literal(EvalValueSerializer),
    Variable(String),
    /// Anonymous user-defined function created by `fn ... -> ...`.
    Function(FormulaFunctionSerializer),
    Let {
        bindings: Box<[(BindingPattern, ExprSerializer)]>,
        body: Box<ExprSerializer>,
    },
    Array(Box<[ExprSerializer]>),
    Object(Box<[(ExprSerializer, ExprSerializer)]>),
    ListComprehension {
        value: Box<ExprSerializer>,
        comprehension: ComprehensionSerializer,
    },
    ObjectComprehension {
        key: Box<ExprSerializer>,
        value: Box<ExprSerializer>,
        comprehension: ComprehensionSerializer,
    },
    Unary(UnaryOperator, Box<ExprSerializer>),
    Binary(Box<ExprSerializer>, BinaryOperator, Box<ExprSerializer>),
    CompareChain(Box<ExprSerializer>, Box<[(BinaryOperator, ExprSerializer)]>),
    Ternary {
        then_expression: Box<ExprSerializer>,
        condition: Box<ExprSerializer>,
        else_expression: Box<ExprSerializer>,
    },
    Call(Box<ExprSerializer>, Box<[ExprSerializer]>),
    Get(Box<ExprSerializer>, String),
    Index(Box<ExprSerializer>, Box<ExprSerializer>),
    Slice {
        object: Box<ExprSerializer>,
        start: Option<Box<ExprSerializer>>,
        end: Option<Box<ExprSerializer>>,
        step: Option<Box<ExprSerializer>>,
    },
}
impl From<&Expr> for ExprSerializer {
    fn from(value: &Expr) -> Self {
        match value {
            Expr::Literal(val) => Self::Literal(EvalValueSerializer::from(val)),
            Expr::Variable(name) => Self::Variable(name.to_string()),
            Expr::Function(func) => Self::Function(FormulaFunctionSerializer::from(func)),
            Expr::Let { bindings, body } => Self::Let {
                bindings: bindings
                    .iter()
                    .map(|(name, value)| (name.clone(), ExprSerializer::from(value)))
                    .collect(),
                body: Box::new(ExprSerializer::from(body.as_ref())),
            },
            Expr::Array(values) => Self::Array(values.iter().map(ExprSerializer::from).collect()),
            Expr::Object(entries) => Self::Object(
                entries
                    .iter()
                    .map(|(key, value)| (ExprSerializer::from(key), ExprSerializer::from(value)))
                    .collect(),
            ),
            Expr::ListComprehension { value, comprehension } => Self::ListComprehension {
                value: Box::new(ExprSerializer::from(value.as_ref())),
                comprehension: ComprehensionSerializer::from(comprehension),
            },
            Expr::ObjectComprehension { key, value, comprehension } => Self::ObjectComprehension {
                key: Box::new(ExprSerializer::from(key.as_ref())),
                value: Box::new(ExprSerializer::from(value.as_ref())),
                comprehension: ComprehensionSerializer::from(comprehension),
            },
            Expr::Unary(op, value) => Self::Unary(*op, Box::new(ExprSerializer::from(value.as_ref()))),
            Expr::Binary(left, op, right) => Self::Binary(
                Box::new(ExprSerializer::from(left.as_ref())),
                *op,
                Box::new(ExprSerializer::from(right.as_ref())),
            ),
            Expr::CompareChain(first, comparisons) => Self::CompareChain(
                Box::new(ExprSerializer::from(first.as_ref())),
                comparisons
                    .iter()
                    .map(|(op, value)| (*op, ExprSerializer::from(value)))
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
            Expr::Ternary { then_expression, condition, else_expression } => Self::Ternary {
                then_expression: Box::new(ExprSerializer::from(then_expression.as_ref())),
                condition: Box::new(ExprSerializer::from(condition.as_ref())),
                else_expression: Box::new(ExprSerializer::from(else_expression.as_ref())),
            },
            Expr::Call(callee, arguments) => Self::Call(
                Box::new(ExprSerializer::from(callee.as_ref())),
                arguments.iter().map(|arg| ExprSerializer::from(arg)).collect(),
            ),
            Expr::Get(object, key) => Self::Get(Box::new(ExprSerializer::from(object.as_ref())), key.to_string()),
            Expr::Index(object, index) => Self::Index(
                Box::new(ExprSerializer::from(object.as_ref())),
                Box::new(ExprSerializer::from(index.as_ref())),
            ),
            Expr::Slice { object, start, end, step } => Self::Slice {
                object: Box::new(Self::from(object.as_ref())),
                start: start.as_ref().map(|expr| Box::new(Self::from(expr.as_ref()))),
                end: end.as_ref().map(|expr| Box::new(Self::from(expr.as_ref()))),
                step: step.as_ref().map(|expr| Box::new(Self::from(expr.as_ref()))),
            },
        }
    }
}
impl TryFrom<ExprSerializer> for Expr {
    type Error = crate::err::Error;
    fn try_from(value: ExprSerializer) -> Result<Self, Self::Error> {
        match value {
            ExprSerializer::Literal(val) => Ok(Self::Literal(EvalValue::try_from(val)?)),
            ExprSerializer::Variable(name) => Ok(Self::Variable(Arc::from(name))),
            ExprSerializer::Function(func) => Ok(Self::Function(FormulaFunction::try_from(func)?)),
            ExprSerializer::Let { bindings, body } => Ok(Self::Let {
                bindings: bindings
                    .into_vec()
                    .into_iter()
                    .map(|(name, value)| Ok((name, Expr::try_from(value)?)))
                    .collect::<Result<Vec<_>, crate::err::Error>>()?
                    .into(),
                body: Arc::new(Expr::try_from(*body)?),
            }),
            ExprSerializer::Array(values) => Ok(Self::Array(values.into_vec().into_iter().map(|expr| Expr::try_from(expr)).collect::<Result<Vec<_>, _>>()?.into())),
            ExprSerializer::Object(entries) => Ok(Self::Object(
                entries
                    .into_vec()
                    .into_iter()
                    .map(|(key, value)| Ok((Expr::try_from(key)?, Expr::try_from(value)?)))
                    .collect::<Result<Vec<(Expr, Expr)>, crate::err::Error>>()?
                    .into(),
            )),
            ExprSerializer::ListComprehension { value, comprehension } => Ok(Self::ListComprehension {
                value: Arc::new(Expr::try_from(*value)?),
                comprehension: Comprehension::try_from(comprehension)?,
            }),
            ExprSerializer::ObjectComprehension { key, value, comprehension } => Ok(Self::ObjectComprehension {
                key: Arc::new(Expr::try_from(*key)?),
                value: Arc::new(Expr::try_from(*value)?),
                comprehension: Comprehension::try_from(comprehension)?,
            }),
            ExprSerializer::Unary(op, value) => Ok(Self::Unary(op, Arc::new(Expr::try_from(*value)?))),
            ExprSerializer::Binary(left, op, right) => Ok(Self::Binary(
                Arc::new(Expr::try_from(*left)?),
                op,
                Arc::new(Expr::try_from(*right)?),
            )),
            ExprSerializer::CompareChain(first, comparisons) => Ok(Self::CompareChain(
                Arc::new(Expr::try_from(*first)?),
                comparisons
                    .into_vec()
                    .into_iter()
                    .map(|(op, value)| Ok((op, Expr::try_from(value)?)))
                    .collect::<Result<Vec<_>, crate::err::Error>>()?
                    .into(),
            )),
            ExprSerializer::Ternary { then_expression, condition, else_expression } => Ok(Self::Ternary {
                then_expression: Arc::new(Expr::try_from(*then_expression)?),
                condition: Arc::new(Expr::try_from(*condition)?),
                else_expression: Arc::new(Expr::try_from(*else_expression)?),
            }),
            ExprSerializer::Call(callee, arguments) => Ok(Self::Call(
                Arc::new(Expr::try_from(*callee)?),
                arguments.into_vec().into_iter().map(|expr| Expr::try_from(expr)).collect::<Result<Vec<_>>>()?.into(),
            )),
            ExprSerializer::Get(object, key) => Ok(Self::Get(Arc::new(Expr::try_from(*object)?), key.into())),
            ExprSerializer::Index(object, index) => Ok(Self::Index(
                Arc::new(Expr::try_from(*object)?),
                Arc::new(Expr::try_from(*index)?),
            )),
            ExprSerializer::Slice { object, start, end, step } => Ok(Self::Slice {
                object: Arc::new(Self::try_from(*object)?),
                start: start.map(|expr| Self::try_from(*expr).map(Arc::new)).transpose()?,
                end: end.map(|expr| Self::try_from(*expr).map(Arc::new)).transpose()?,
                step: step.map(|expr| Self::try_from(*expr).map(Arc::new)).transpose()?,
            }),
        }
    }
}

/// Stores the shared loop clause used by list and dictionary comprehensions.
#[derive(Clone, Debug, PartialEq)]
pub struct Comprehension {
    targets: Arc<[BindingPattern]>,
    iterable: Arc<Expr>,
    filter: Option<Arc<Expr>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComprehensionSerializer {
    targets: Box<[BindingPattern]>,
    iterable: Box<ExprSerializer>,
    filter: Option<Box<ExprSerializer>>,
}
impl From<&Comprehension> for ComprehensionSerializer {
    fn from(value: &Comprehension) -> Self {
        Self {
            targets: value.targets.iter().cloned().collect(),
            iterable: Box::new(ExprSerializer::from(value.iterable.as_ref())),
            filter: value.filter.as_ref().map(|expr| Box::new(ExprSerializer::from(expr.as_ref()))),
        }
    }
}
impl TryFrom<ComprehensionSerializer> for Comprehension {
    type Error = crate::err::Error;
    fn try_from(value: ComprehensionSerializer) -> Result<Self, Self::Error> {
        Ok(Self {
            targets: value.targets.into(),
            iterable: Arc::new(Expr::try_from(*value.iterable)?),
            filter: value.filter.map(|expr| Ok::<Arc<Expr>, crate::err::Error>(Arc::new(Expr::try_from(*expr)?))).transpose()?,
        })
    }
}

impl FormulaFunction {
    /// Returns the distinct free names referenced by this function, in source traversal order.
    fn free_variables(&self) -> IndexSet<Arc<str>> {
        let mut bound = IndexSet::new();
        bound.insert("args".into());
        if let Some(parameters) = &self.parameters {
            for pattern in parameters.iter() { pattern.collect_names(&mut bound); }
        }
        let mut free = IndexSet::new();
        self.body.collect_free_variables(&bound, &mut free);
        free
    }
}

impl Expr {
    /// Traverses an expression and records names not bound by its surrounding lexical scopes.
    fn collect_free_variables(
        &self,
        bound: &IndexSet<Arc<str>>,
        free: &mut IndexSet<Arc<str>>,
    ) {
        match self {
            Self::Literal(_) => {}
            Self::Variable(name) => {
                if !bound.contains(name) {
                    free.insert(name.clone());
                }
            }
            Self::Function(function) => {
                // Keep the outer scope visible while introducing the nested function's own
                // parameters. This lets an outer closure retain values needed only by a child.
                let mut nested_bound = bound.clone();
                nested_bound.insert("args".into());
                if let Some(parameters) = &function.parameters {
                    for pattern in parameters.iter() { pattern.collect_names(&mut nested_bound); }
                }
                function
                    .body
                    .collect_free_variables(&nested_bound, free);
            }
            Self::Let { bindings, body } => {
                let mut local_bound = bound.clone();
                for (name, value) in bindings.iter() {
                    value.collect_free_variables(&local_bound, free);
                    name.collect_names(&mut local_bound);
                }
                body.collect_free_variables(&local_bound, free);
            }
            Self::Array(values) => {
                for value in values.iter() {
                    value.collect_free_variables(bound, free);
                }
            }
            Self::Object(entries) => {
                for (key, value) in entries.iter() {
                    key.collect_free_variables(bound, free);
                    value.collect_free_variables(bound, free);
                }
            }
            Self::ListComprehension {
                value,
                comprehension,
            } => {
                comprehension
                    .iterable
                    .collect_free_variables(bound, free);
                let mut local_bound = bound.clone();
                for pattern in comprehension.targets.iter() {
                    pattern.collect_names(&mut local_bound);
                }
                if let Some(filter) = &comprehension.filter {
                    filter.collect_free_variables(&local_bound, free);
                }
                value.collect_free_variables(&local_bound, free);
            }
            Self::ObjectComprehension {
                key,
                value,
                comprehension,
            } => {
                comprehension
                    .iterable
                    .collect_free_variables(bound, free);
                let mut local_bound = bound.clone();
                for pattern in comprehension.targets.iter() {
                    pattern.collect_names(&mut local_bound);
                }
                if let Some(filter) = &comprehension.filter {
                    filter.collect_free_variables(&local_bound, free);
                }
                key.collect_free_variables(&local_bound, free);
                value.collect_free_variables(&local_bound, free);
            }
            Self::Unary(_, value) => value.collect_free_variables(bound, free),
            Self::Binary(left, _, right) => {
                left.collect_free_variables(bound, free);
                right.collect_free_variables(bound, free);
            }
            Self::CompareChain(first, comparisons) => {
                first.collect_free_variables(bound, free);
                for (_, value) in comparisons.iter() {
                    value.collect_free_variables(bound, free);
                }
            }
            Self::Ternary {
                then_expression,
                condition,
                else_expression,
            } => {
                then_expression.collect_free_variables(bound, free);
                condition.collect_free_variables(bound, free);
                else_expression.collect_free_variables(bound, free);
            }
            Self::Call(callee, arguments) => {
                callee.collect_free_variables(bound, free);
                for argument in arguments.iter() {
                    argument.collect_free_variables(bound, free);
                }
            }
            Self::Get(object, _) => object.collect_free_variables(bound, free),
            Self::Index(object, index) => {
                object.collect_free_variables(bound, free);
                index.collect_free_variables(bound, free);
            }
            Self::Slice { object, start, end, step } => {
                object.collect_free_variables(bound, free);
                for expr in [start, end, step].into_iter().flatten() {
                    expr.collect_free_variables(bound, free);
                }
            }
        }
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal(val) => write!(f, "{val}"),
            Self::Variable(name) => write!(f, "{name}"),
            Self::Function(func) => write!(f, "fn {} -> ({})", func.parameters.as_ref().map_or("".to_string(), |params| format_patterns(params)), func.body),
            Self::Let {bindings, body} => {
                write!(f, "let {} for ({})",
                    bindings.iter().map(|(name, value)| format!("{name} = {value}")).collect::<Vec<_>>().join(", "),
                    body
                )
            },
            Self::Array(values) => {
                write!(f, "[{}]", values.iter().map(|value| format!("{value}")).collect::<Vec<_>>().join(", "))
            }
            Self::Object(entries) => {
                write!(f, "{{{}}}", entries.iter().map(|(key, value)| format!("{key}: {value}")).collect::<Vec<_>>().join(", "))
            },
            Self::ListComprehension { value, comprehension } => {
                write!(f, "[({}) for {} in ({}){}]",
                    value,
                    format_patterns(&comprehension.targets),
                    comprehension.iterable,
                    if let Some(filter) = &comprehension.filter {
                        format!(" if ({filter})")
                    } else {
                        "".to_string()
                    }
                )
            },
            Self::ObjectComprehension { key, value, comprehension } => {
                write!(f, "{{({}): ({}) for {} in ({}){}}}",
                    key,
                    value,
                    format_patterns(&comprehension.targets),
                    comprehension.iterable,
                    if let Some(filter) = &comprehension.filter {
                        format!(" if ({filter})")
                    } else {
                        "".to_string()
                    }
                )
            },
            Self::Unary(op, value) => {
                let op_str = match op {
                    UnaryOperator::Positive => "+",
                    UnaryOperator::Negative => "-",
                    UnaryOperator::Not => "!",
                    UnaryOperator::BitwiseNot => "~",
                };
                write!(f, "{op_str}({value})")
            },
            Self::Binary(left, op, right) => {
                let op_str = op.symbol();
                write!(f, "({left}) {op_str} ({right})")
            },
            Self::CompareChain(first, comparisons) => {
                write!(f, "({first})")?;
                for (op, value) in comparisons.iter() {
                    let op_str = op.symbol();
                    write!(f, " {op_str} ({value})")?;
                }
                Ok(())
            },
            Self::Ternary { then_expression, condition, else_expression } => {
                write!(f, "({then_expression}) if ({condition}) else ({else_expression})")
            },
            Self::Call(callee, arguments) => {
                write!(f, "({callee})({})", arguments.iter().map(|arg| format!("{arg}")).collect::<Vec<_>>().join(", "))
            },
            Self::Get(object, property) => {
                write!(f, "({object}).{property}")
            },
            Self::Index(object, index) => {
                write!(f, "({object})[{index}]")
            },
            Self::Slice { object, start, end, step } => {
                write!(f, "({object})[")?;
                if let Some(start) = start { write!(f, "{start}")?; }
                write!(f, ":")?;
                if let Some(end) = end { write!(f, "{end}")?; }
                if let Some(step) = step { write!(f, ":{step}")?; }
                write!(f, "]")
            },
        }
    }
}

/// Lists unary operators supported by the formula language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Copy)]
pub enum UnaryOperator {
    Positive,
    Negative,
    Not,
    BitwiseNot,
}

/// Lists binary operators supported by the formula language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Copy)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    FloorDivide,
    Modulo,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    NotIn,
    BitAnd,
    BitXor,
    BitOr,
    And,
    Or,
    LeftShift,
    RightShift,
    LogicRightShift,
    Xor,
    Coalesce,
    StrictEqual,
    StrictNotEqual,
}

impl BinaryOperator {
    /// Returns the source-language spelling used when an expression is serialized or shown
    /// in an error context. This must remain total because diagnostics must never panic.
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::FloorDivide => "//",
            Self::Modulo => "%",
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::GreaterEqual => ">=",
            Self::In => "in",
            Self::NotIn => "not in",
            Self::BitAnd => "&",
            Self::BitXor => "^",
            Self::BitOr => "|",
            Self::And => "and",
            Self::Or => "or",
            Self::Xor => "xor",
            Self::Coalesce => "??",
            Self::LeftShift => "<<",
            Self::RightShift => ">>",
            Self::LogicRightShift => ">>>",
            Self::StrictEqual => "===",
            Self::StrictNotEqual => "!==",
        }
    }
}

/// Represents one lexical token emitted by the formula tokenizer.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Int(i64),
    Float(f64),
    String(String),
    Identifier(String),
    Plus,
    Minus,
    Star,
    Slash,
    FloorSlash,
    Percent,
    Ampersand,
    AndAnd,
    Caret,
    Pipe,
    OrOr,
    Equal,
    EqualEqual,
    EqualEqualEqual,
    BangEqual,
    BangEqualEqual,
    Bang,
    QuestionQuestion,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Comma,
    Colon,
    Dot,
    Arrow,
    End,
    Tilde,
    LeftShift,
    RightShift,
    LogicRightShift,
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Int(i) => write!(f, "{i}"),
            Token::Float(flt) => write!(f, "{flt}"),
            Token::String(s) => write!(f, "{s}"),
            Token::Identifier(ident) => write!(f, "{ident}"),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Slash => write!(f, "/"),
            Token::FloorSlash => write!(f, "//"),
            Token::Percent => write!(f, "%"),
            Token::Ampersand => write!(f, "&"),
            Token::AndAnd => write!(f, "&&"),
            Token::Caret => write!(f, "^"),
            Token::Pipe => write!(f, "|"),
            Token::OrOr => write!(f, "||"),
            Token::Equal => write!(f, "="),
            Token::EqualEqual => write!(f, "=="),
            Token::EqualEqualEqual => write!(f, "==="),
            Token::BangEqual => write!(f, "!="),
            Token::BangEqualEqual => write!(f, "!=="),
            Token::Bang => write!(f, "!"),
            Token::QuestionQuestion => write!(f, "??"),
            Token::Less => write!(f, "<"),
            Token::LessEqual => write!(f, "<="),
            Token::Greater => write!(f, ">"),
            Token::GreaterEqual => write!(f, ">="),
            Token::LeftParen => write!(f, "("),
            Token::RightParen => write!(f, ")"),
            Token::LeftBracket => write!(f, "["),
            Token::RightBracket => write!(f, "]"),
            Token::LeftBrace => write!(f, "{{"),
            Token::RightBrace => write!(f, "}}"),
            Token::Comma => write!(f, ","),
            Token::Colon => write!(f, ":"),
            Token::Dot => write!(f, "."),
            Token::Arrow => write!(f, "->"),
            Token::End => write!(f, "<end>"),
            Token::Tilde => write!(f, "~"),
            Token::LeftShift => write!(f, "<<"),
            Token::RightShift => write!(f, ">>"),
            Token::LogicRightShift => write!(f, ">>>"),
        }
    }
}

/// Converts a formula source string into a token stream.
pub struct Tokenizer {
    chars: Vec<char>,
    position: usize,
    pending: VecDeque<Token>,
}

impl Tokenizer {
    /// Creates a tokenizer over `source` at character offset zero.
    pub fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            position: 0,
            pending: VecDeque::new(),
        }
    }

    /// Tokenizes the complete formula source.
    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token()?;
            let is_end = token == Token::End;
            tokens.push(token);
            if is_end {
                break;
            }
        }
        Ok(tokens)
    }

    /// Scans and returns the next token from the formula source.
    fn next_token(&mut self) -> Result<Token> {
        if let Some(token) = self.pending.pop_front() {
            return Ok(token);
        }
        while self
            .chars
            .get(self.position)
            .is_some_and(|character| character.is_whitespace())
        {
            self.position += 1;
        }
        let Some(character) = self.chars.get(self.position).copied() else {
            return Ok(Token::End);
        };

        if character == '#' {
            let start = self.position;
            self.position += 1;
            return self.radix_integer(16, start);
        }
        if character.is_ascii_digit() {
            return self.number_or_dice();
        }
        if character == '\'' || character == '"' {
            return self.string(character);
        }
        if character == '$' {
            if self
                .chars
                .get(self.position + 1)
                .is_some_and(|character| is_word(*character))
            {
                self.position += 1;
                return self.identifier();
            }
            return Err(Error::Lexical { position: self.position, kind: LexError::UnexpectedCharacter('$') });
        }
        if character == '_' || character.is_alphabetic() {
            return self.identifier();
        }

        self.position += 1;
        Ok(match character {
            '+' => Token::Plus,
            '-' => {
                if self.chars.get(self.position) == Some(&'>') {
                    self.position += 1;
                    Token::Arrow
                } else {
                    Token::Minus
                }
            }
            '*' => Token::Star,
            '%' => Token::Percent,
            '&' => {
                if self.chars.get(self.position) == Some(&'&') {
                    self.position += 1;
                    Token::AndAnd
                } else {
                    Token::Ampersand
                }
            }
            '^' => Token::Caret,
            '|' => {
                if self.chars.get(self.position) == Some(&'|') {
                    self.position += 1;
                    Token::OrOr
                } else {
                    Token::Pipe
                }
            }
            '(' => Token::LeftParen,
            ')' => Token::RightParen,
            '[' => Token::LeftBracket,
            ']' => Token::RightBracket,
            '{' => Token::LeftBrace,
            '}' => Token::RightBrace,
            ',' => Token::Comma,
            ':' => Token::Colon,
            '.' => Token::Dot,
            '~' => Token::Tilde,
            '?' => {
                if self.chars.get(self.position) == Some(&'?') {
                    self.position += 1;
                    Token::QuestionQuestion
                } else {
                    return Err(Error::Lexical { position: self.position - 1, kind: LexError::LoneQuestionMark });
                }
            }
            '/' => {
                if self.chars.get(self.position) == Some(&'/') {
                    self.position += 1;
                    Token::FloorSlash
                } else {
                    Token::Slash
                }
            }
            '=' => {
                if self.chars.get(self.position) == Some(&'=') {
                    self.position += 1;
                    if self.chars.get(self.position) == Some(&'=') {
                        self.position += 1;
                        Token::EqualEqualEqual
                    } else {
                        Token::EqualEqual
                    }
                } else {
                    Token::Equal
                }
            }
            '!' => {
                if self.chars.get(self.position) == Some(&'=') {
                    self.position += 1;
                    if self.chars.get(self.position) == Some(&'=') {
                        self.position += 1;
                        Token::BangEqualEqual
                    } else {
                        Token::BangEqual
                    }
                } else {
                    Token::Bang
                }
            }
            '<' => {
                let n = self.chars.get(self.position);
                if n == Some(&'=') {
                    self.position += 1;
                    Token::LessEqual
                } else if n == Some(&'<') {
                    self.position += 1;
                    Token::LeftShift
                } else {
                    Token::Less
                }
            }
            '>' => {
                let n = self.chars.get(self.position);
                if n == Some(&'=') {
                    self.position += 1;
                    Token::GreaterEqual
                } else if n == Some(&'>') {
                    self.position += 1;
                    let m = self.chars.get(self.position);
                    if m == Some(&'>') {
                        self.position += 1;
                        Token::LogicRightShift
                    } else {
                        Token::RightShift
                    }
                } else {
                    Token::Greater
                }
            }
            _ => return Err(Error::Lexical { position: self.position - 1, kind: LexError::UnexpectedCharacter(character) }),
        })
    }

    /// Reads digits after a radix prefix, applying identical validation to `#` and `0x`.
    fn radix_integer(&mut self, radix: u32, literal_start: usize) -> Result<Token> {
        let digits_start = self.position;
        while self
            .chars
            .get(self.position)
            .is_some_and(|character| character.to_digit(radix).is_some())
        {
            self.position += 1;
        }
        if self.position == digits_start {
            return Err(Error::Lexical { position: self.position, kind: LexError::MissingDigits { radix } });
        }
        if self
            .chars
            .get(self.position)
            .is_some_and(|character| character.is_ascii_alphanumeric() || *character == '_')
        {
            return Err(Error::Lexical { position: self.position, kind: LexError::InvalidDigit { radix } });
        }
        let digits: String = self.chars[digits_start..self.position].iter().collect();
        let value = i64::from_str_radix(&digits, radix).map_err(|source| {
            let literal: String = self.chars[literal_start..self.position].iter().collect();
            Error::Lexical { position: literal_start, kind: LexError::InvalidInteger { literal, source } }
        })?;
        Ok(Token::Int(value))
    }

    /// Tokenizes an integer, float, radix-prefixed integer, or dice notation.
    fn number_or_dice(&mut self) -> Result<Token> {
        let start = self.position;

        if self.chars.get(start) == Some(&'0') {
            if let Some(prefix) = self.chars.get(start + 1).copied() {
                let radix = match prefix {
                    'x' | 'X' => Some(16),
                    'b' | 'B' => Some(2),
                    'o' | 'O' => Some(8),
                    _ => None,
                };
                if let Some(radix) = radix {
                    self.position += 2;
                    return self.radix_integer(radix, start);
                }
            }
        }

        while self
            .chars
            .get(self.position)
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.position += 1;
        }

        let integer_text: String = self.chars[start..self.position].iter().collect();
        if self.chars.get(self.position) == Some(&'d')
            && self
                .chars
                .get(self.position + 1)
                .is_some_and(|character| character.is_ascii_digit())
        {
            self.position += 1;
            let sides_start = self.position;
            while self
                .chars
                .get(self.position)
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.position += 1;
            }
            let sides_text: String = self.chars[sides_start..self.position].iter().collect();
            let number = Self::parse_integer(&integer_text, start)?;
            let sides = Self::parse_integer(&sides_text, sides_start)?;
            self.pending.extend([
                Token::LeftParen,
                Token::Int(number),
                Token::Comma,
                Token::Int(sides),
                Token::RightParen,
            ]);
            return Ok(Token::Identifier("dice".to_owned()));
        }

        if self.chars.get(self.position) == Some(&'.')
            && self
                .chars
                .get(self.position + 1)
                .is_some_and(|character| character.is_ascii_digit())
        {
            self.position += 1;
            while self
                .chars
                .get(self.position)
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.position += 1;
            }
            let text: String = self.chars[start..self.position].iter().collect();
            return text.parse::<f64>().map(Token::Float).map_err(|source| Error::Lexical {
                position: start, kind: LexError::InvalidFloat { literal: text, source },
            });
        }

        Ok(Token::Int(Self::parse_integer(&integer_text, start)?))
    }

    fn parse_integer(literal: &str, position: usize) -> Result<i64> {
        literal.parse().map_err(|source| Error::Lexical {
            position, kind: LexError::InvalidInteger { literal: literal.to_owned(), source },
        })
    }

    /// Tokenizes a quoted string literal.
    fn string(&mut self, quote: char) -> Result<Token> {
        let quote_start = self.position;
        if let Some(quote_end) = quoted_formula_literal_end(&self.chars, quote_start) {
            let output = self.chars[quote_start + 1..quote_end].iter().collect();
            self.position = quote_end + 1;
            return Ok(Token::String(output));
        }

        self.position += 1;
        let mut output = String::new();
        while let Some(character) = self.chars.get(self.position).copied() {
            self.position += 1;
            if character == quote {
                return Ok(Token::String(output));
            }
            if character == '\\' {
                let escaped = self
                    .chars
                    .get(self.position)
                    .copied()
                    .ok_or_else(|| Error::Lexical { position: self.position - 1, kind: LexError::IncompleteEscape })?;
                self.position += 1;
                output.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    other => other,
                });
            } else {
                output.push(character);
            }
        }
        Err(Error::Lexical { position: quote_start, kind: LexError::UnterminatedString })
    }

    /// Tokenizes an identifier or reserved keyword.
    fn identifier(&mut self) -> Result<Token> {
        let start = self.position;
        while self
            .chars
            .get(self.position)
            .is_some_and(|character| is_word(*character))
        {
            self.position += 1;
        }
        if start == self.position {
            return Err(Error::Lexical { position: start, kind: LexError::ExpectedIdentifier });
        }
        Ok(Token::Identifier(
            self.chars[start..self.position].iter().collect(),
        ))
    }
}

/// Builds a formula expression tree from a token stream.
pub struct Parser {
    tokens: Vec<Token>,
    position: usize,
    tokenizer: Option<Tokenizer>,
    token_ends: Vec<usize>,
    lexical_error: Option<crate::err::Error>,
}

impl Parser {
    /// Creates a parser positioned before the first token.
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            position: 0,
            tokenizer: None,
            token_ends: Vec::new(),
            lexical_error: None,
        }
    }

    /// Parses the complete input into its typed representation.
    pub fn parse(mut self) -> Result<Expr> {
        let expression = self.parse_expression()?;
        if !matches!(self.peek(), Token::End) {
            return Err(Error::Syntax {
                token_index: self.position,
                kind: SyntaxError::TrailingToken { found: self.peek().clone() },
            });
        }
        Ok(expression)
    }

    /// Parses a complete expression according to formula precedence rules.
    fn parse_expression(&mut self) -> Result<Expr> {
        self.parse_conditional()
    }

    /// Parses `fn x, y -> expression` into an ordinary first-class function value.
    ///
    /// Empty parameter lists are written `fn -> expression`. Named parameters must be
    /// distinct identifiers and may not reuse formula-language reserved words.
    fn parse_function(&mut self) -> Result<Expr> {
        let mut parameters = Vec::new();
        let mut names = HashSet::new();
        if !matches!(self.peek(), Token::Arrow) {
            loop {
                parameters.push(self.parse_binding_pattern(&mut names, true, false)?);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.advance();
            }
        }
        self.expect(Token::Arrow)?;
        let body = self.parse_expression()?;
        Ok(Expr::Function(FormulaFunction {
            parameters: Some(parameters.into()),
            body: Arc::new(body),
        }))
    }

    /// Parses `let x = value, y = value for expression`.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_let(&mut self) -> Result<Expr> {
        let mut bindings = Vec::new();
        let mut names = HashSet::new();
        loop {
            let pattern = self.parse_binding_pattern(&mut names, false, false)?;
            self.expect(Token::Equal)?;
            let value = self.parse_expression()?;
            bindings.push((pattern, value));
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.advance();
        }
        self.expect_identifier("for")?;
        let body = self.parse_expression()?;
        Ok(Expr::Let {
            bindings: bindings.into(),
            body: Arc::from(body),
        })
    }

    /// Parses nested list patterns, allowing rest captures only within a list context.
    fn parse_binding_pattern(
        &mut self,
        names: &mut HashSet<String>,
        parameter: bool,
        allow_rest: bool,
    ) -> Result<BindingPattern> {
        let token_index = self.position;
        match self.advance().clone() {
            Token::Identifier(name) => {
                if is_reserved_binding(&name) || (parameter && name == "args") {
                    return Err(Error::Syntax { token_index, kind: SyntaxError::InvalidBinding { name } });
                }
                if name != "_" && !names.insert(name.clone()) {
                    return Err(Error::Syntax { token_index, kind: SyntaxError::DuplicateBinding { name } });
                }
                Ok(BindingPattern::Name(name))
            }
            Token::Star if allow_rest => {
                let token_index = self.position;
                let found = self.advance().clone();
                let Token::Identifier(rest) = found else {
                    return Err(Error::Syntax {
                        token_index,
                        kind: SyntaxError::ExpectedBinding { found },
                    });
                };
                if is_reserved_binding(&rest) || (parameter && rest == "args") {
                    return Err(Error::Syntax { token_index, kind: SyntaxError::InvalidBinding { name: rest } });
                }
                if rest != "_" && !names.insert(rest.clone()) {
                    return Err(Error::Syntax { token_index, kind: SyntaxError::DuplicateBinding { name: rest } });
                }
                Ok(BindingPattern::Rest { rest })
            }
            Token::LeftBracket => {
                let mut items = Vec::new();
                let mut has_rest = false;
                if !matches!(self.peek(), Token::RightBracket) {
                    loop {
                        let token_index = self.position;
                        let pattern = self.parse_binding_pattern(names, parameter, true)?;
                        if matches!(pattern, BindingPattern::Rest { .. })
                            && std::mem::replace(&mut has_rest, true)
                        {
                            return Err(Error::Syntax { token_index, kind: SyntaxError::MultipleRestPatterns });
                        }
                        items.push(pattern);
                        if !self.match_token(Token::Comma)
                            || matches!(self.peek(), Token::RightBracket)
                        {
                            break;
                        }
                    }
                }
                self.expect(Token::RightBracket)?;
                Ok(BindingPattern::List(items))
            }
            found => Err(Error::Syntax { token_index, kind: SyntaxError::ExpectedBinding { found } }),
        }
    }

    /// Parses conditional according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_conditional(&mut self) -> Result<Expr> {
        let then_expression = self.parse_coalesce()?;
        if self.match_identifier("if") {
            let condition = self.parse_coalesce()?;
            self.expect_identifier("else")?;
            let else_expression = self.parse_expression()?;
            Ok(Expr::Ternary {
                then_expression: Arc::from(then_expression),
                condition: Arc::from(condition),
                else_expression: Arc::from(else_expression),
            })
        } else {
            Ok(then_expression)
        }
    }

    /// Null fallback binds below OR and associates to the right.
    fn parse_coalesce(&mut self) -> Result<Expr> {
        let left = self.parse_or()?;
        if self.match_token(Token::QuestionQuestion) {
            Ok(Expr::Binary(
                Arc::new(left),
                BinaryOperator::Coalesce,
                Arc::new(self.parse_coalesce()?),
            ))
        } else {
            Ok(left)
        }
    }

    /// Parses or according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_or(&mut self) -> Result<Expr> {
        let mut expression = self.parse_xor()?;
        while self.match_identifier("or") || self.match_token(Token::OrOr) {
            expression = Expr::Binary(
                Arc::from(expression),
                BinaryOperator::Or,
                Arc::from(self.parse_xor()?),
            );
        }
        Ok(expression)
    }

    /// Parses logical XOR between the OR and AND precedence levels.
    fn parse_xor(&mut self) -> Result<Expr> {
        let mut expression = self.parse_and()?;
        while self.match_identifier("xor") {
            expression = Expr::Binary(
                Arc::from(expression),
                BinaryOperator::Xor,
                Arc::from(self.parse_and()?),
            );
        }
        Ok(expression)
    }

    /// Parses and according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_and(&mut self) -> Result<Expr> {
        let mut expression = self.parse_not()?;
        while self.match_identifier("and") || self.match_token(Token::AndAnd) {
            expression = Expr::Binary(
                Arc::from(expression),
                BinaryOperator::And,
                Arc::from(self.parse_not()?),
            );
        }
        Ok(expression)
    }

    /// Parses not according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_not(&mut self) -> Result<Expr> {
        if self.match_identifier("not") {
            Ok(Expr::Unary(
                UnaryOperator::Not,
                Arc::from(self.parse_not()?),
            ))
        } else {
            self.parse_comparison()
        }
    }

    /// Parses comparison according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_comparison(&mut self) -> Result<Expr> {
        let first = self.parse_bit_or()?;
        let mut comparisons = Vec::new();
        loop {
            let is_not_in = matches!(self.peek(), Token::Identifier(value) if value == "not")
                && matches!(self.peek_n(1), Token::Identifier(next) if next == "in");
            let operator = match self.peek() {
                Token::EqualEqual => Some(BinaryOperator::Equal),
                Token::BangEqual => Some(BinaryOperator::NotEqual),
                Token::Less => Some(BinaryOperator::Less),
                Token::LessEqual => Some(BinaryOperator::LessEqual),
                Token::Greater => Some(BinaryOperator::Greater),
                Token::GreaterEqual => Some(BinaryOperator::GreaterEqual),
                Token::EqualEqualEqual => Some(BinaryOperator::StrictEqual),
                Token::BangEqualEqual => Some(BinaryOperator::StrictNotEqual),
                Token::Identifier(value) if value == "in" => Some(BinaryOperator::In),
                Token::Identifier(value) if value == "not" && is_not_in => {
                    self.advance();
                    Some(BinaryOperator::NotIn)
                }
                _ => None,
            };
            let Some(operator) = operator else {
                break;
            };
            self.advance();
            comparisons.push((operator, self.parse_bit_or()?));
        }
        if comparisons.is_empty() {
            Ok(first)
        } else {
            Ok(Expr::CompareChain(Arc::from(first), comparisons.into()))
        }
    }

    /// Parses bitwise OR according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_bit_or(&mut self) -> Result<Expr> {
        let mut expression = self.parse_bit_xor()?;
        while matches!(self.peek(), Token::Pipe) {
            self.advance();
            expression = Expr::Binary(
                Arc::from(expression),
                BinaryOperator::BitOr,
                Arc::from(self.parse_bit_xor()?),
            );
        }
        Ok(expression)
    }

    /// Parses bitwise XOR according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_bit_xor(&mut self) -> Result<Expr> {
        let mut expression = self.parse_bit_and()?;
        while matches!(self.peek(), Token::Caret) {
            self.advance();
            expression = Expr::Binary(
                Arc::from(expression),
                BinaryOperator::BitXor,
                Arc::from(self.parse_bit_and()?),
            );
        }
        Ok(expression)
    }

    /// Parses bitwise AND according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_bit_and(&mut self) -> Result<Expr> {
        let mut expression = self.parse_shift()?;
        while matches!(self.peek(), Token::Ampersand) {
            self.advance();
            expression = Expr::Binary(
                Arc::from(expression),
                BinaryOperator::BitAnd,
                Arc::from(self.parse_shift()?),
            );
        }
        Ok(expression)
    }

    /// Parses bitwise shifts according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_shift(&mut self) -> Result<Expr> {
        let mut expression = self.parse_sum()?;
        loop {
            let operator = match self.peek() {
                Token::LeftShift => Some(BinaryOperator::LeftShift),
                Token::RightShift => Some(BinaryOperator::RightShift),
                Token::LogicRightShift => Some(BinaryOperator::LogicRightShift),
                _ => None,
            };
            let Some(operator) = operator else {
                break;
            };
            self.advance();
            expression = Expr::Binary(
                Arc::from(expression),
                operator,
                Arc::from(self.parse_sum()?),
            );
        }
        Ok(expression)
    }

    /// Parses sum according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_sum(&mut self) -> Result<Expr> {
        let mut expression = self.parse_term()?;
        loop {
            let operator = match self.peek() {
                Token::Plus => Some(BinaryOperator::Add),
                Token::Minus => Some(BinaryOperator::Subtract),
                _ => None,
            };
            let Some(operator) = operator else {
                break;
            };
            self.advance();
            expression = Expr::Binary(
                Arc::from(expression),
                operator,
                Arc::from(self.parse_term()?),
            );
        }
        Ok(expression)
    }

    /// Parses term according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_term(&mut self) -> Result<Expr> {
        let mut expression = self.parse_unary()?;
        loop {
            let operator = match self.peek() {
                Token::Star => Some(BinaryOperator::Multiply),
                Token::Slash => Some(BinaryOperator::Divide),
                Token::FloorSlash => Some(BinaryOperator::FloorDivide),
                Token::Percent => Some(BinaryOperator::Modulo),
                _ => None,
            };
            let Some(operator) = operator else {
                break;
            };
            self.advance();
            expression = Expr::Binary(
                Arc::from(expression),
                operator,
                Arc::from(self.parse_unary()?),
            );
        }
        Ok(expression)
    }

    /// Parses unary according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_unary(&mut self) -> Result<Expr> {
        match self.peek() {
            Token::Bang => {
                self.advance();
                Ok(Expr::Unary(
                    UnaryOperator::Not,
                    Arc::from(self.parse_unary()?),
                ))
            }
            Token::Plus => {
                self.advance();
                Ok(Expr::Unary(
                    UnaryOperator::Positive,
                    Arc::from(self.parse_unary()?),
                ))
            }
            Token::Minus => {
                self.advance();
                Ok(Expr::Unary(
                    UnaryOperator::Negative,
                    Arc::from(self.parse_unary()?),
                ))
            }
            Token::Tilde => {
                self.advance();
                Ok(Expr::Unary(
                    UnaryOperator::BitwiseNot,
                    Arc::from(self.parse_unary()?),
                ))
            }
            _ => self.parse_postfix(),
        }
    }

    /// Parses postfix according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expression = self.parse_primary()?;
        loop {
            match self.peek() {
                Token::LeftParen => {
                    self.advance();
                    let mut arguments = Vec::new();
                    if !matches!(self.peek(), Token::RightParen) {
                        loop {
                            arguments.push(self.parse_expression()?);
                            if matches!(self.peek(), Token::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(Token::RightParen)?;
                    expression = Expr::Call(Arc::from(expression), arguments.into());
                }
                Token::Dot => {
                    self.advance();
                    let token_index = self.position;
                    let property = match self.advance().clone() {
                        Token::Identifier(property) => Index::String(property.into()),
                        Token::Int(index) if index >= 0 => Index::Int(index),
                        found => return Err(Error::Syntax { token_index, kind: SyntaxError::ExpectedProperty { found } }),
                    };
                    expression = Expr::Get(Arc::from(expression), property);
                }
                Token::LeftBracket => {
                    self.advance();
                    let start = if matches!(self.peek(), Token::Colon) {
                        None
                    } else {
                        Some(Arc::new(self.parse_expression()?))
                    };
                    if self.match_token(Token::Colon) {
                        let end = if matches!(self.peek(), Token::Colon | Token::RightBracket) {
                            None
                        } else {
                            Some(Arc::new(self.parse_expression()?))
                        };
                        let step = if self.match_token(Token::Colon) && !matches!(self.peek(), Token::RightBracket) {
                            Some(Arc::new(self.parse_expression()?))
                        } else {
                            None
                        };
                        self.expect(Token::RightBracket)?;
                        expression = Expr::Slice { object: Arc::new(expression), start, end, step };
                    } else {
                        self.expect(Token::RightBracket)?;
                        expression = Expr::Index(Arc::new(expression), start.expect("an index was parsed"));
                    }
                }
                _ => break,
            }
        }
        Ok(expression)
    }

    /// Parses primary according to formula precedence rules.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_primary(&mut self) -> Result<Expr> {
        let token_index = self.position;
        match self.advance().clone() {
            // Recognize expression keywords wherever an operand is accepted.
            // Their bodies still consume a full expression, preserving scope.
            Token::Identifier(value) if value == "let" => self.parse_let(),
            Token::Identifier(value) if value == "fn" => self.parse_function(),
            Token::Int(value) => Ok(Expr::Literal(EvalValue::Int(value))),
            Token::Float(value) => Ok(Expr::Literal(EvalValue::Float(value))),
            Token::String(value) => Ok(Expr::Literal(EvalValue::String(value.into()))),
            Token::Identifier(value) if value == "True" || value == "true" => {
                Ok(Expr::Literal(EvalValue::Bool(true)))
            }
            Token::Identifier(value) if value == "False" || value == "false" => {
                Ok(Expr::Literal(EvalValue::Bool(false)))
            }
            Token::Identifier(value) if value == "None" || value == "null" => {
                Ok(Expr::Literal(EvalValue::Null))
            }
            Token::Identifier(value) => Ok(Expr::Variable(Arc::from(value))),
            Token::LeftParen => {
                let expression = self.parse_expression()?;
                self.expect(Token::RightParen)?;
                Ok(expression)
            }
            Token::LeftBracket => {
                if matches!(self.peek(), Token::RightBracket) {
                    self.advance();
                    return Ok(Expr::Array(Arc::from(Vec::new())));
                }
                let first = self.parse_expression()?;
                if self.match_identifier("for") {
                    let (targets, iterable, filter) = self.parse_comprehension_clause()?;
                    self.expect(Token::RightBracket)?;
                    return Ok(Expr::ListComprehension {
                        value: Arc::from(first),
                        comprehension: Comprehension {
                            targets: targets.into(),
                            iterable: Arc::from(iterable),
                            filter: filter.map(Arc::from),
                        },
                    });
                }
                let mut values = vec![first];
                while matches!(self.peek(), Token::Comma) {
                    self.advance();
                    if matches!(self.peek(), Token::RightBracket) {
                        break;
                    }
                    values.push(self.parse_expression()?);
                }
                self.expect(Token::RightBracket)?;
                Ok(Expr::Array(Arc::from(values)))
            }
            Token::LeftBrace => {
                if matches!(self.peek(), Token::RightBrace) {
                    self.advance();
                    return Ok(Expr::Object(Vec::new().into()));
                }
                let first_key = self.parse_expression()?;
                self.expect(Token::Colon)?;
                let first_value = self.parse_expression()?;
                if self.match_identifier("for") {
                    let (targets, iterable, filter) = self.parse_comprehension_clause()?;
                    self.expect(Token::RightBrace)?;
                    return Ok(Expr::ObjectComprehension {
                        key: Arc::from(first_key),
                        value: Arc::from(first_value),
                        comprehension: Comprehension {
                            targets: targets.into(),
                            iterable: Arc::from(iterable),
                            filter: filter.map(Arc::from),
                        },
                    });
                }
                let mut entries = vec![(first_key, first_value)];
                while matches!(self.peek(), Token::Comma) {
                    self.advance();
                    if matches!(self.peek(), Token::RightBrace) {
                        break;
                    }
                    let key = self.parse_expression()?;
                    self.expect(Token::Colon)?;
                    let value = self.parse_expression()?;
                    entries.push((key, value));
                }
                self.expect(Token::RightBrace)?;
                Ok(Expr::Object(Arc::from(entries)))
            }
            found => Err(Error::Syntax { token_index, kind: SyntaxError::ExpectedExpression { found } }),
        }
    }

    /// Parses the variable binding, iterable, and optional filter of a comprehension.
    ///
    /// Required fields and variant-specific JSON types are checked immediately. Formula
    /// strings are preserved for runtime evaluation.
    fn parse_comprehension_clause(&mut self) -> Result<(Vec<BindingPattern>, Expr, Option<Expr>)> {
        let mut targets = Vec::new();
        let mut names = HashSet::new();
        let mut has_rest = false;
        let start = self.position;
        loop {
            let token_index = self.position;
            let target = self.parse_binding_pattern(&mut names, false, true)?;
            if matches!(target, BindingPattern::Rest { .. })
                && std::mem::replace(&mut has_rest, true)
            {
                return Err(Error::Syntax { token_index, kind: SyntaxError::MultipleRestPatterns });
            }
            targets.push(target);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.advance();
        }
        if targets.len() == 1 && has_rest {
            return Err(Error::Syntax {
                token_index: start,
                kind: SyntaxError::ExpectedBinding { found: Token::Star },
            });
        }
        self.expect_identifier("in")?;
        let iterable = self.parse_coalesce()?;
        let filter = if self.match_identifier("if") {
            Some(self.parse_coalesce()?)
        } else {
            None
        };
        Ok((targets, iterable, filter))
    }

    /// Returns the current token without consuming it.
    fn peek(&mut self) -> &Token {
        self.peek_n(0)
    }

    /// Returns a later token without consuming the stream.
    fn peek_n(&mut self, offset: usize) -> &Token {
        self.fill_tokens(self.position + offset);
        self.tokens
            .get(self.position + offset)
            .unwrap_or(&Token::End)
    }

    /// Lazily supplies lookahead for source parsing, retaining each token's end.
    fn fill_tokens(&mut self, position: usize) {
        while self.tokens.len() <= position {
            let Some(tokenizer) = self.tokenizer.as_mut() else {
                break;
            };
            let token = match tokenizer.next_token() {
                Ok(token) => token,
                Err(error) => {
                    // A lexical error only matters if the grammar requires this
                    // token. At an expression boundary it belongs to the suffix.
                    self.lexical_error = Some(error);
                    Token::End
                }
            };
            let is_end = token == Token::End;
            self.token_ends.push(tokenizer.position);
            self.tokens.push(token);
            if is_end {
                self.tokenizer = None;
            }
        }
    }

    /// Consumes and returns the current token.
    fn advance(&mut self) -> &Token {
        let position = self.position;
        self.fill_tokens(position);
        if self.position < self.tokens.len() {
            self.position += 1;
        }
        self.tokens.get(position).unwrap_or(&Token::End)
    }

    /// Consumes an identifier when it matches the requested text.
    fn match_identifier(&mut self, expected: &str) -> bool {
        if matches!(self.peek(), Token::Identifier(value) if value == expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consumes a token when it matches the requested kind.
    fn match_token(&mut self, expected: Token) -> bool {
        if self.peek() == &expected {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consumes an identifier or returns a parser error.
    fn expect_identifier(&mut self, expected: &str) -> Result<()> {
        if self.match_identifier(expected) {
            Ok(())
        } else {
            Err(Error::Syntax {
                token_index: self.position,
                kind: SyntaxError::ExpectedKeyword { expected: expected.to_owned(), found: self.peek().clone() },
            })
        }
    }

    /// Consumes an expected token or returns a parser error.
    fn expect(&mut self, expected: Token) -> Result<()> {
        if self.peek() == &expected {
            self.advance();
            Ok(())
        } else {
            Err(Error::Syntax {
                token_index: self.position,
                kind: SyntaxError::ExpectedToken { expected, found: self.peek().clone() },
            })
        }
    }
}

/// Returns whether a formula word cannot be used as a local binding name.
fn is_reserved_binding(name: &str) -> bool {
    matches!(
        name,
        "let"
            | "for"
            | "in"
            | "if"
            | "else"
            | "and"
            | "or"
            | "xor"
            | "not"
            | "True"
            | "False"
            | "None"
            | "true"
            | "false"
            | "null"
            | "fn"
    )
}

#[allow(unused)]
pub mod builtins {
    use std::{cmp::Ordering, sync::Arc};

    use crate::formula::{
        expect_arity, expect_arity_between, full_formula, get_path, has_path, indexof_object, indexof_str, minmax,
        mul_values, normalize_index, pop_values, power_values, python_round, range_values, shuffle,
        sum_values, zip_arrays, Env, EvalValue, FormulaEngine, Function,
    };
    use crate::err::{bail, error, Error, Result, ResultContext};
    use indexmap::IndexMap;
    use rand::distr::weighted::Weight;
use rand::{Rng, RngExt};
    use std::sync::LazyLock as Lazy;

    fn build_env(env: &mut Env, name: &str, func: fn(&[EvalValue], &mut FormulaEngine, &Env) -> Result<EvalValue>) {
        let (key, f) = EvalValue::func(name, func);
        env.insert(key, f);
    }
    fn build_dict(dict: &mut IndexMap<Arc<str>, EvalValue>, typ: &str, name: &str, func: fn(&[EvalValue], &mut FormulaEngine, &Env) -> Result<EvalValue>) {
        let key: Arc<str> = Arc::from(format!("{typ}_{name}"));
        dict.insert(name.into(), EvalValue::Func(Function::Builtin(key, func).into()));
    }
    fn insert_registry(dict: &mut IndexMap<Arc<str>, EvalValue>, reg: &IndexMap<Arc<str>, EvalValue>) {
        for value in reg.values() {
            if let EvalValue::Func(function) = value {
                if let Function::Builtin(key, _) = function.as_ref() {
                    dict.insert(key.clone(), value.clone());
                }
            }
        }
    }

    /////// GLOBALS \\\\\\\
    ///
    pub static GLOBAL_ENV: Lazy<Env<'static>> = Lazy::new(|| {
        let mut env = Env::new();

        build_env(&mut env, "dice", dice);
        build_env(&mut env, "min", min);
        build_env(&mut env, "max", max);
        build_env(&mut env, "abs", abs);
        build_env(&mut env, "pow", pow);
        build_env(&mut env, "round", round);
        build_env(&mut env, "str", fn_str);
        build_env(&mut env, "int", int);
        build_env(&mut env, "float", float);
        build_env(&mut env, "bool", fn_bool);
        build_env(&mut env, "zip", zip);
        build_env(&mut env, "len", len);
        build_env(&mut env, "type", fn_type);
        build_env(&mut env, "exists", exists);
        build_env(&mut env, "fromentries", fromentries);
        build_env(&mut env, "random", random);
        build_env(&mut env, "range", range);
        build_env(&mut env, "eval", eval);
        build_env(&mut env, "error", error);
        build_env(&mut env, "sum", array_sum);
        build_env(&mut env, "mul", array_mul);
        build_env(&mut env, "indexof", indexof);
        build_env(&mut env, "enum", array_enum);
        build_env(&mut env, "flatten", array_flatten);
        build_env(&mut env, "map", array_map);
        build_env(&mut env, "filter", array_filter);
        build_env(&mut env, "drop", array_drop);
        build_env(&mut env, "cumul", array_cumul);
        build_env(&mut env, "reduce", array_reduce);
        build_env(&mut env, "apply", apply);
        build_env(&mut env, "nan", nan);
        build_env(&mut env, "inf", inf);
        build_env(&mut env, "order", order);
        build_env(&mut env, "try", fn_try);
        build_env(&mut env, "method", method);

        env
    });

    pub fn dice(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("dice", &arguments, 2)?;
        let number = arguments[0].to_i64()?;
        let sides = arguments[1].to_i64()?;
        if number < 0 || sides <= 0 {
            bail!(InvalidArgument, "dice requires a nonnegative dice count and a positive number of sides")
        }
        let mut total = 0_i64;
        for _ in 0..number {
            total = total
                .checked_add(formula.rng.random_range(1..=sides))
                .ok_or_else(|| error!(Overflow, "Numeric overflow while rolling dice"))?;
        }
        Ok(EvalValue::Int(total))
    }
    pub fn min(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        minmax(&arguments, Ordering::Less)
    }
    pub fn max(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        minmax(&arguments, Ordering::Greater)
    }
    pub fn abs(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("abs", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Int(value) => Ok(EvalValue::Int(value.checked_abs().ok_or_else(|| Error::Overflow("Overflow in function abs".into()))?)),
            EvalValue::Float(value) => Ok(EvalValue::Float(value.abs())),
            _ => bail!(Type, "abs requires a number"),
        }
    }
    pub fn pow(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("pow", &arguments, 2)?;
        power_values(&arguments[0], &arguments[1])
    }
    pub fn round(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("round", arguments.len(), 1, 2)?;
        let value = arguments[0].to_f64()?;
        let digits = if arguments.len() == 2 {
            arguments[1].to_i64()?
        } else {
            0
        };
        let rounded = python_round(value, digits)?;
        if digits <= 0 {
            Ok(EvalValue::Int(rounded as i64))
        } else {
            Ok(EvalValue::Float(rounded))
        }
    }
    pub fn fn_str(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("str", &arguments, 1)?;
        Ok(EvalValue::String(arguments[0].to_string().into()))
    }
    pub fn int(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("int", &arguments, 1)?;
        Ok(EvalValue::Int(arguments[0].to_i64()?))
    }
    pub fn float(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("float", &arguments, 1)?;
        Ok(EvalValue::Float(arguments[0].to_f64()?))
    }
    pub fn fn_bool(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("bool", &arguments, 1)?;
        Ok(EvalValue::Bool(arguments[0].truthy()))
    }
    pub fn zip(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        let mut args = Vec::new();
        for argument in arguments.iter() {
            match argument {
                EvalValue::Array(values) => args.push(values.clone()),
                _ => bail!(Type, "zip requires lists as arguments"),
            }
        }
        if args.is_empty() {
            bail!(Type, "zip requires at least one list");
        }
        Ok(zip_arrays(&args))
    }
    pub fn len(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("len", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => Ok(EvalValue::Int(values.len() as i64)),
            EvalValue::Object(entries) => Ok(EvalValue::Int(entries.len() as i64)),
            EvalValue::String(value) => Ok(EvalValue::Int(value.chars().count() as i64)),
            _ => bail!(Type, "len requires a list, dictionary, or string"),
        }
    }
    pub fn fn_type(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("type", &arguments, 1)?;
        Ok(EvalValue::String(arguments[0].type_name()))
    }
    /// Tests if there is a variable with the name given as the argument within the environment.
    pub fn exists(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("exists", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::String(name) => Ok(EvalValue::Bool(env.get(name).is_some())),
            _ => bail!(Type, "exists requires a string"),
        }
    }
    pub fn fromentries(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("fromentries", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(entries) => {
                let mut obj = IndexMap::new();
                for entry in entries.iter() {
                    match entry {
                        EvalValue::Array(pair) if pair.len() == 2 => {
                            let key = pair[0].clone().try_into()?;
                            obj.insert(key, pair[1].clone());
                        }
                        _ => bail!(Type, "fromentries requires a list of pairs"),
                    }
                }
                Ok(EvalValue::Object(Arc::from(obj)))
            }
            _ => bail!(Type, "fromentries requires a list"),
        }
    }
    pub fn random(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("random", &arguments, 0)?;
        Ok(EvalValue::Float(formula.rng.random_range(0.0..=1.0)))
    }
    pub fn range(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        range_values(&arguments.iter().collect::<Vec<_>>())
    }
    pub fn eval(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("eval", arguments.len(), 1, 2)?;
        let expr = &arguments[0];
        let expression = match expr {
            EvalValue::String(s) => s.clone(),
            _ => return Ok(expr.clone()),
        };
        let local_env = if arguments.len() == 2 {
            match &arguments[1] {
                EvalValue::Object(obj) => Env::from_index_map(&**obj),
                _ => bail!(Type, "eval requires a dictionary as its second argument"),
            }
        } else {
            Env::new()
        };
        let local_env = env.merged(&local_env);
        if let Some(full) = full_formula(&expression) {
            formula.evaluate(full, &local_env)
        } else {
            Ok(EvalValue::String(formula.format(expression, &local_env)?))
        }
    }
    pub fn error(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        if let Some(msg) = arguments.get(0) {
            bail!(User, "{}", msg.repr());
        } else {
            bail!(User, "Error!")
        }
    }
    /// Finds a value in a list, substring in a string, or value in a dictionary.
    pub fn indexof(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("indexof", arguments, 2)?;
        match &arguments[0] {
            EvalValue::Array(_) => array_indexof(arguments, formula, env),
            EvalValue::String(haystack) => {
                let EvalValue::String(needle) = &arguments[1] else {
                    bail!(Type, "indexof requires a string to search for in a string")
                };
                Ok(indexof_str(haystack, needle))
            }
            EvalValue::Object(values) => Ok(indexof_object(values, &arguments[1])),
            value => bail!(Type,
                "indexof cannot search in {}: a list, string, or dictionary is required",
                value.diagnostic()
            ),
        }
    }

    /// Calls a function with every item from a list as one positional argument.
    pub fn apply(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("apply", arguments, 2)?;
        let EvalValue::Array(values) = &arguments[1] else {
            bail!(Type, "apply requires a list as its second argument")
        };
        arguments[0].clone().call(values, formula, env)
    }

    /// Returns a NaN (Not a Number) float value.
    pub fn nan(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("nan", arguments, 0)?;
        Ok(EvalValue::Float(f64::NAN))
    }
    /// Returns an infinite float value.
    pub fn inf(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("inf", arguments, 0)?;
        Ok(EvalValue::Float(f64::INFINITY))
    }
    /// Compares two numeric values and returns an integer indicating their order.
    /// Returns a negative integer if the first is less than the second,
    /// zero if they are equal, and a positive integer if the first is greater.
    /// Both arguments must be numeric (either integers or floats).
    /// Uses total ordering for floating-point comparisons.
    pub fn order(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("order", arguments, 2)?;
        let a = &arguments[0];
        let b = &arguments[1];
        Ok(EvalValue::Int(a.total_cmp(b) as i64))
    }
    /// Tries to call a function and optionally handles errors.
    /// The first argument must be a function.
    /// The second argument, if provided, must be a function that takes an error dictionary.
    /// Returns the result of the first function if it succeeds, otherwise calls the error handler or returns `null`.
    pub fn fn_try(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        if arguments.len() < 1 || arguments.len() > 2 {
            return Err(Error::Arity { 
                function: "try".into(), 
                expected: crate::err::ArgumentCount::Between { min: 1, max: 2 }, 
                actual: arguments.len() 
            })
        }
        let f = &arguments[0];
        match f {
            EvalValue::Func(_) => {},
            _ => bail!(Type, "try requires a function as its first argument"),
        }
        let e = if arguments.len() == 2 {
            Some(&arguments[1])
        } else {
            None
        };

        let res = f.clone().call(&[], formula, env);
        match res {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Some(e) = e {
                    match e {
                        EvalValue::Func(_) => {},
                        _ => bail!(Type, "try requires a function as its second argument"),
                    }
                    return e.clone().call(&[err.to_dict()], formula, env);
                }
                Ok(EvalValue::Null)
            }
        }
        
    }

    /// Look up a method on a value and returns it if found (null otherwise).
    /// Usefull in case a field in a dictionary shadows a registered method.
    pub fn method(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("method", arguments, 2)?;
        let obj = &arguments[0];
        let met = match &arguments[1] {
            EvalValue::String(s) => s.clone(),
            _ => bail!(Type, "method requires a string as its second argument"),
        };
        let reg = match obj.method_registry() {
            Some(reg) => reg,
            None => return Ok(EvalValue::Null),
        };
        match reg.get(&met) {
            Some(m) => Ok(m.clone()),
            None => Ok(EvalValue::Null),
        }
    }

    ////// ARRAY METHODS \\\\\\\
        pub static ARRAY_METHODS: Lazy<IndexMap<Arc<str>, EvalValue>> = Lazy::new(|| {
            let mut map = IndexMap::new();
            build_dict(&mut map, "list", "sum", array_sum);
            build_dict(&mut map, "list", "mul", array_mul);
            build_dict(&mut map, "list", "indexof", array_indexof);
            build_dict(&mut map, "list", "enum", array_enum);
            build_dict(&mut map, "list", "flatten", array_flatten);
            build_dict(&mut map, "list", "flat", array_flat);
            build_dict(&mut map, "list", "len", array_len);
            build_dict(&mut map, "list", "unique", array_unique);
            build_dict(&mut map, "list", "sort", array_sort);
            build_dict(&mut map, "list", "sortby", array_sort_by);
            build_dict(&mut map, "list", "minby", array_min_by);
            build_dict(&mut map, "list", "maxby", array_max_by);
            build_dict(&mut map, "list", "extremeby", array_extreme_by);
            build_dict(&mut map, "list", "groupby", array_group_by);
            build_dict(&mut map, "list", "reverse", array_reverse);
            build_dict(&mut map, "list", "take", array_take);
            build_dict(&mut map, "list", "skip", array_skip);
            build_dict(&mut map, "list", "slice", array_slice);
            build_dict(&mut map, "list", "chunks", array_chunks);
            build_dict(&mut map, "list", "windows", array_windows);
            build_dict(&mut map, "list", "all", array_all);
            build_dict(&mut map, "list", "any", array_any);
            build_dict(&mut map, "list", "join", array_join);
            build_dict(&mut map, "list", "choice", array_choice);
            build_dict(&mut map, "list", "weighted", array_weighted);
            build_dict(&mut map, "list", "shuffle", array_shuffle);
            build_dict(&mut map, "list", "coalesce", array_coalesce);
            build_dict(&mut map, "list", "tryget", array_tryget);
            build_dict(&mut map, "list", "pop", array_pop);
            build_dict(&mut map, "list", "map", array_map);
            build_dict(&mut map, "list", "filter", array_filter);
            build_dict(&mut map, "list", "drop", array_drop);
            build_dict(&mut map, "list", "cumul", array_cumul);
            build_dict(&mut map, "list", "reduce", array_reduce);
            build_dict(&mut map, "list", "apply", array_apply);
            build_dict(&mut map, "list", "find", array_find);
            build_dict(&mut map, "list", "fmt", array_fmt);
            build_dict(&mut map, "list", "max", array_max);
            build_dict(&mut map, "list", "min", array_min);
            map
        });

    pub fn array_sum(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("sum", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => sum_values(values),
            _ => bail!(Type, "sum requires a list"),
        }
    }
    pub fn array_mul(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("mul", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => mul_values(values),
            _ => bail!(Type, "mul requires a list"),
        }
    }
    pub fn array_indexof(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("indexof", &arguments, 2)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                let target = &arguments[1];
                for (i, value) in values.iter().enumerate() {
                    if value == target {
                        return Ok(EvalValue::Int(i as i64));
                    }
                }
                Ok(EvalValue::Null)
            }
            _ => bail!(Type, "indexof requires a list as its first argument"),
        }
    }
    pub fn array_enum(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("enum", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                let mut enumerated = Vec::new();
                for (i, value) in values.iter().enumerate() {
                    enumerated.push(EvalValue::Array(Arc::from(vec![
                        EvalValue::Int(i as i64),
                        value.clone(),
                    ])));
                }
                Ok(EvalValue::Array(Arc::from(enumerated)))
            }
            _ => bail!(Type, "enum requires a list"),
        }
    }
    pub fn array_flatten(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("flatten", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                let mut flattened = Vec::new();
                for value in values.iter() {
                    match value {
                        EvalValue::Array(inner) => {
                            flattened.extend(inner.iter().cloned());
                        }
                        _ => flattened.push(value.clone()),
                    }
                }
                Ok(EvalValue::Array(Arc::from(flattened)))
            }
            _ => bail!(Type, "flatten requires a list"),
        }
    }
    /// Flattens all nested lists in source order without recursive Rust calls.
    pub fn array_flat(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("flat", arguments, 1)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "flat requires a list");
        };
        let mut flattened = Vec::new();
        let mut stack = vec![values.iter()];
        while let Some(items) = stack.last_mut() {
            match items.next() {
                Some(EvalValue::Array(inner)) => stack.push(inner.iter()),
                Some(value) => flattened.push(value.clone()),
                None => {
                    stack.pop();
                }
            }
        }
        Ok(EvalValue::Array(flattened.into()))
    }

    pub fn array_len(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("len", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => Ok(EvalValue::Int(values.len() as i64)),
            _ => bail!(Type, "len requires a list"),
        }
    }
    pub fn array_unique(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("unique", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                let mut seen = Vec::new();
                for value in values.iter() {
                    if !seen.contains(value) {
                        seen.push(value.clone());
                    }
                }
                Ok(EvalValue::Array(Arc::from(seen)))
            }
            _ => bail!(Type, "unique requires a list"),
        }
    }
    pub fn array_sort(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("sort", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                let mut sorted = values.to_vec();
                sorted.sort_by(EvalValue::total_cmp);
                Ok(EvalValue::Array(Arc::from(sorted)))
            }
            _ => bail!(Type, "sort requires a list"),
        }
    }
    /// Evaluates each key exactly once, in input order, with the usual value/index callback.
    fn keyed_values(
        name: &str,
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<Vec<(EvalValue, EvalValue)>> {
        expect_arity(name, arguments, 2)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "{name} requires a list");
        };
        if !matches!(arguments[1], EvalValue::Func(_)) {
            bail!(Type, "{name} requires a key function");
        }
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let key = arguments[1]
                    .clone()
                    .call(&[value.clone(), EvalValue::Int(index as i64)], formula, env)
                    .with_context(|| format!("Key for {name} at element {index}"))?;
                Ok((key, value.clone()))
            })
            .collect()
    }

    /// A fallible stable merge sort propagates incomparable-key errors without panicking.
    fn sort_keyed(mut values: Vec<(EvalValue, EvalValue)>) -> Result<Vec<(EvalValue, EvalValue)>> {
        if values.len() < 2 {
            return Ok(values);
        }
        let right = values.split_off(values.len() / 2);
        let mut left = sort_keyed(values)?.into_iter().peekable();
        let mut right = sort_keyed(right)?.into_iter().peekable();
        let mut result = Vec::with_capacity(left.len() + right.len());
        while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
            if super::compare_values(&a.0, &b.0)? != Ordering::Greater {
                result.push(left.next().unwrap());
            } else {
                result.push(right.next().unwrap());
            }
        }
        result.extend(left);
        result.extend(right);
        Ok(result)
    }

    pub fn array_sort_by(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        let values = keyed_values("sortby", arguments, formula, env)?;
        for (key, _) in &values {
            super::compare_values(key, key)?;
        }
        let sorted = sort_keyed(values).context("Comparing sortby keys")?;
        Ok(EvalValue::Array(
            sorted.into_iter().map(|(_, value)| value).collect(),
        ))
    }

    /// Returns the extreme element (min or max) of an array based on a key function.
    /// Uses total ordering for comparison of keys
    fn array_extreme_by_inner(
        name: &str,
        direction: Ordering,
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        let values = keyed_values(name, arguments, formula, env)?;
        let mut values = values.into_iter();
        let Some(mut best) = values.next() else {
            bail!(InvalidArgument, "{name} requires a nonempty list");
        };
        for candidate in values {
            if candidate.0.total_cmp(&best.0) == direction
            {
                best = candidate;
            }
        }
        Ok(best.1)
    }

    pub fn array_extreme_by(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        let values = keyed_values("extremeby", arguments, formula, env)?;
        let mut values = values.into_iter();
        let Some(mut mn) = values.next() else {
            bail!(InvalidArgument, "extremeby requires a nonempty list");
        };
        let mut mx = mn.clone();
        for candidate in values {
            if candidate.0.total_cmp(&mn.0) == Ordering::Less {
                mn = candidate;
            }
            else if candidate.0.total_cmp(&mx.0) == Ordering::Greater {
                mx = candidate;
            }
        }
        Ok(EvalValue::Array(Arc::from(vec![mn.1, mx.1])))
    }

    pub fn array_min_by(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        array_extreme_by_inner("minby", Ordering::Less, arguments, formula, env)
    }

    pub fn array_max_by(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        array_extreme_by_inner  ("maxby", Ordering::Greater, arguments, formula, env)
    }

    pub fn array_group_by(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        let values = keyed_values("groupby", arguments, formula, env)?;
        let mut groups: IndexMap<Arc<str>, Vec<EvalValue>> = IndexMap::new();
        for (key, value) in values {
            let EvalValue::String(key) = key else {
                bail!(Type,
                    "groupby requires keys of type str, got {}",
                    key.diagnostic()
                );
            };
            groups.entry(key).or_default().push(value);
        }
        Ok(EvalValue::Object(Arc::new(
            groups
                .into_iter()
                .map(|(key, values)| (key, EvalValue::Array(values.into())))
                .collect(),
        )))
    }

    pub fn array_reverse(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("reverse", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                let mut reversed = values.to_vec();
                reversed.reverse();
                Ok(EvalValue::Array(Arc::from(reversed)))
            }
            _ => bail!(Type, "reverse requires a list"),
        }
    }
    pub fn array_take(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("take", &arguments, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "take requires a list as its first argument"),
        };
        let count = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "take requires an integer as its second argument"),
        };
        if count < 0 {
            bail!(InvalidArgument, "take requires a nonnegative integer as its second argument")
        }
        Ok(EvalValue::Array(
            values.iter().take(count as usize).cloned().collect(),
        ))
    }
    pub fn array_skip(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("skip", &arguments, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "skip requires a list as its first argument"),
        };
        let count = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "skip requires an integer as its second argument"),
        };
        if count < 0 {
            bail!(InvalidArgument, "skip requires a nonnegative integer as its second argument")
        }
        Ok(EvalValue::Array(
            values.iter().skip(count as usize).cloned().collect(),
        ))
    }
    pub fn array_slice(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("slice", &arguments, 3)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "slice requires a list as its first argument"),
        };
        let start = arguments[1].to_i64()?;
        let end = arguments[2].to_i64()?;
        if start < 0 {
            bail!(InvalidArgument, "slice requires nonnegative integers as its first and second arguments")
        }
        let end = if end < 0 {
            values.len() as i64 + end
        } else {
            end
        };
        if end < start {
            bail!(InvalidArgument, "slice requires a first argument less than or equal to the second")
        }
        Ok(EvalValue::Array(
            values
                .iter()
                .skip(start as usize)
                .take((end - start) as usize)
                .cloned()
                .collect(),
        ))
    }
    pub fn array_chunks(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("chunks", &arguments, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "chunks requires a list as its first argument"),
        };
        let size = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "chunks requires an integer as its second argument"),
        };
        if size <= 0 {
            bail!(InvalidArgument, "chunks requires a positive integer as its second argument")
        }
        Ok(EvalValue::Array(
            values
                .chunks(size as usize)
                .map(|chunk| EvalValue::Array(Arc::from(chunk.to_vec())))
                .collect(),
        ))
    }
    pub fn array_windows(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("windows", &arguments, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "windows requires a list as its first argument"),
        };
        let size = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "windows requires an integer as its second argument"),
        };
        if size <= 0 {
            bail!(InvalidArgument, "windows requires a positive integer as its second argument")
        }
        Ok(EvalValue::Array(
            values
                .windows(size as usize)
                .map(|window| EvalValue::Array(Arc::from(window.to_vec())))
                .collect(),
        ))
    }
    pub fn array_all(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        array_quantify("all", true, arguments, formula, env)
    }
    pub fn array_any(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        array_quantify("any", false, arguments, formula, env)
    }

    fn array_quantify(
        name: &str,
        all: bool,
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between(name, arguments.len(), 1, 2)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "{name} requires a list");
        };
        let predicate = arguments.get(1);
        if predicate.is_some_and(|value| !matches!(value, EvalValue::Func(_))) {
            bail!(Type, "{name} requires a predicate function");
        }
        for (index, value) in values.iter().enumerate() {
            let truth = if let Some(predicate) = predicate {
                predicate
                    .clone()
                    .call(&[value.clone(), EvalValue::Int(index as i64)], formula, env)
                    .with_context(|| format!("Predicate for {name} at element {index}"))?
                    .truthy()
            } else {
                value.truthy()
            };
            if truth != all {
                return Ok(EvalValue::Bool(!all));
            }
        }
        Ok(EvalValue::Bool(all))
    }
    pub fn array_join(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("join", &arguments, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "join requires a list as its first argument"),
        };
        let separator = match &arguments[1] {
            EvalValue::String(s) => s,
            _ => bail!(Type, "join requires a string as its second argument"),
        };
        let to_joined: Vec<String> = values
            .iter()
            .map(|value| String::from(value.repr().as_ref()))
            .collect::<Vec<_>>();
        Ok(EvalValue::String(to_joined.join(separator).into()))
    }
    pub fn array_choice(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("choice", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                if values.is_empty() {
                    bail!(InvalidArgument, "choice requires a nonempty list")
                }
                let index = formula.rng.random_range(0..values.len());
                Ok(values[index].clone())
            }
            _ => bail!(Type, "choice requires a list"),
        }
    }
    pub fn array_weighted(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("weighted", &arguments, 1)?;
        match &arguments[0] {
            EvalValue::Array(values) => {
                if values.is_empty() {
                    bail!(InvalidArgument, "weighted requires a nonempty list")
                }
                let mut total_weight = 0.0;
                let mut weighted_values = Vec::new();
                for value in values.iter() {
                    match value {
                        EvalValue::Array(pair) if pair.len() == 2 => {
                            let weight = pair[1].to_f64()?;
                            if weight.is_nan() || weight.is_infinite() {
                                bail!(InvalidArgument, "weighted requires finite weights")
                            }
                            if weight < 0.0 {
                                bail!(InvalidArgument, "weighted_choice requires nonnegative weights")
                            }
                            total_weight += weight;
                            if !total_weight.is_finite() {
                                bail!(InvalidArgument, "weighted requires finite weights")
                            }
                            weighted_values.push((pair[0].clone(), weight));
                        }
                        _ => bail!(Type, "weighted_choice requires a list of pairs"),
                    }
                }
                if total_weight == 0.0 {
                    bail!(InvalidArgument, "weighted_choice requires a positive total weight")
                }
                let mut rng_value = formula.rng.random_range(0.0..total_weight);
                for (value, weight) in weighted_values.iter() {
                    if rng_value < *weight {
                        return Ok(value.clone());
                    }
                    rng_value -= *weight;
                }
                unreachable!()
            }
            _ => bail!(Type, "weighted_choice requires a list"),
        }
    }
    pub fn array_shuffle(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("shuffle", &arguments, 1)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "shuffle requires a list as its first argument"),
        };
        let mut shuffled = values.clone();
        shuffle(Arc::make_mut(&mut shuffled), &mut formula.rng);
        Ok(EvalValue::Array(shuffled))
    }
    pub fn array_coalesce(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("coalesce", &arguments, 1)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "coalesce requires a list as its first argument"),
        };
        for value in values.iter() {
            if value != &EvalValue::Null {
                return Ok(value.clone());
            }
        }
        Ok(EvalValue::Null)
    }
    pub fn array_tryget(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("tryget", arguments.len(), 2, 3)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "tryget requires a list as its first argument"),
        };
        let arguments = &arguments[1..];
        let mut index = match &arguments[0] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "tryget requires an integer as its second argument"),
        };
        if index < 0 {
            index = values.len() as i64 + index;
        }
        if index < 0 || index >= values.len() as i64 {
            let default = arguments.get(1).cloned().unwrap_or(EvalValue::Null);
            Ok(default)
        } else {
            Ok(values[index as usize].clone())
        }
    }
    pub fn array_pop(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("pop", arguments.len(), 1, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "pop requires a list as its first argument"),
        };
        let arguments = &arguments[1..];
        let index = arguments
            .get(0)
            .map(|v| match v {
                EvalValue::Int(i) => Ok(*i),
                _ => bail!(Type, "pop requires an integer as its second argument"),
            })
            .transpose()?
            .map(|index| normalize_index(index, values.len()))
            .transpose()?;
        pop_values(&values.iter().collect::<Vec<_>>(), index)
    }

    /// Applies a callback to every list item. The callback receives value and index.
    pub fn array_map(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("map", arguments, 2)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "map requires a list as its first argument")
        };
        let function = arguments[1].clone();
        let mut mapped = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            mapped.push(function.clone().call(
                &[value.clone(), EvalValue::Int(index as i64)],
                formula,
                env,
            )?);
        }
        Ok(EvalValue::Array(mapped.into()))
    }

    /// Keeps list items for which the callback returns a true value.
    pub fn array_filter(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("filter", arguments, 2)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "filter requires a list as its first argument")
        };
        let function = arguments[1].clone();
        let mut filtered = Vec::new();
        for (index, value) in values.iter().enumerate() {
            if function
                .clone()
                .call(&[value.clone(), EvalValue::Int(index as i64)], formula, env)?
                .truthy()
            {
                filtered.push(value.clone());
            }
        }
        Ok(EvalValue::Array(filtered.into()))
    }

    /// Removes the last `count` values from a list.
    pub fn array_drop(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("skip", &arguments, 2)?;
        let values = match &arguments[0] {
            EvalValue::Array(values) => values,
            _ => bail!(Type, "skip requires a list as its first argument"),
        };
        let count = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "skip requires an integer as its second argument"),
        };
        if count < 0 {
            bail!(InvalidArgument, "skip requires a nonnegative integer as its second argument")
        }
        let rev_count = values.len() as i64 - count;
        if rev_count < 0 {
            return Ok(EvalValue::Array(Vec::new().into()));
        }
        Ok(EvalValue::Array(
            values.iter().take(rev_count as usize).cloned().collect(),
        ))
    }

    /// Returns every intermediate accumulator produced by a callback.
    ///
    /// The callback receives accumulator, current value, and index. Without an explicit
    /// initial value the first list item is both the initial accumulator and the first output.
    pub fn array_cumul(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("cumul", arguments.len(), 2, 3)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "cumul requires a list as its first argument")
        };
        let function = arguments[1].clone();
        let (mut accumulator, start, mut output) = match arguments.get(2) {
            Some(initial) => (initial.clone(), 0, Vec::with_capacity(values.len())),
            None => {
                let Some(first) = values.first() else {
                    return Ok(EvalValue::Array(Vec::new().into()));
                };
                (first.clone(), 1, vec![first.clone()])
            }
        };
        for (index, value) in values.iter().enumerate().skip(start) {
            accumulator = function.clone().call(
                &[accumulator, value.clone(), EvalValue::Int(index as i64)],
                formula,
                env,
            )?;
            output.push(accumulator.clone());
        }
        Ok(EvalValue::Array(output.into()))
    }

    /// Folds a list into one value with an optional initial accumulator.
    pub fn array_reduce(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("reduce", arguments.len(), 2, 3)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "reduce requires a list as its first argument")
        };
        let function = arguments[1].clone();
        let (mut accumulator, start) = match arguments.get(2) {
            Some(initial) => (initial.clone(), 0),
            None => (
                values.first().cloned().ok_or_else(|| {
                    error!(InvalidArgument, "reduce requires an initial value for an empty list")
                })?,
                1,
            ),
        };
        for (index, value) in values.iter().enumerate().skip(start) {
            accumulator = function.clone().call(
                &[accumulator, value.clone(), EvalValue::Int(index as i64)],
                formula,
                env,
            )?;
        }
        Ok(accumulator)
    }

    /// Calls a function with the list's values as positional arguments.
    pub fn array_apply(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("apply", arguments, 2)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "apply requires a list as its receiver")
        };
        arguments[1].clone().call(values, formula, env)
    }

    /// Returns the first list item accepted by a callback, or an optional default.
    pub fn array_find(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("find", arguments.len(), 2, 3)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "find requires a list as its first argument")
        };
        let function = arguments[1].clone();
        for (index, value) in values.iter().enumerate() {
            if function
                .clone()
                .call(&[value.clone(), EvalValue::Int(index as i64)], formula, env)?
                .truthy()
            {
                return Ok(value.clone());
            }
        }
        Ok(arguments.get(2).cloned().unwrap_or(EvalValue::Null))
    }
    /// Iter through a list of values, transforming each values into a str. If a value is a func, it is evaluated with no arguments.
    pub fn array_fmt(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("fmt", arguments, 1)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "fmt requires a list as its first argument")
        };
        let mut formatted = Vec::with_capacity(values.len());
        for value in values.iter() {
            let formatted_value = match value {
                EvalValue::Func(_) => value.clone().call(&[], formula, env)?.repr(),
                _ => value.repr(),
            };
            formatted.push(formatted_value);
        }
        let final_string = formatted
            .iter()
            .map(|s| s.as_ref())
            .collect::<Vec<_>>()
            .join("");
        Ok(EvalValue::String(final_string.into()))
    }

    pub fn array_max(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("max", arguments, 1)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "max requires a list as its first argument")
        };
        if values.is_empty() {
            bail!(InvalidArgument, "max requires a nonempty list")
        }
        let max_value = values
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap();
        Ok(max_value.clone())
    }
    pub fn array_min(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("min", arguments, 1)?;
        let EvalValue::Array(values) = &arguments[0] else {
            bail!(Type, "min requires a list as its first argument")
        };
        if values.is_empty() {
            bail!(InvalidArgument, "min requires a nonempty list")
        }
        let min_value = values
            .iter()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap();
        Ok(min_value.clone())
    }

    //////// OBJECT METHODS \\\\\\\
    pub static OBJECT_METHODS: Lazy<IndexMap<Arc<str>, EvalValue>> = Lazy::new(|| {
        let mut map = IndexMap::new();
        build_dict(&mut map, "dict", "len", object_len);
        build_dict(&mut map, "dict", "indexof", object_indexof);
        build_dict(&mut map, "dict", "keys", object_keys);
        build_dict(&mut map, "dict", "values", object_values);
        build_dict(&mut map, "dict", "entries", object_entries);
        build_dict(&mut map, "dict", "haspath", object_haspath);
        build_dict(&mut map, "dict", "getpath", object_getpath);
        build_dict(&mut map, "dict", "tryget", object_tryget);
        build_dict(&mut map, "dict", "remove", object_remove);
        build_dict(&mut map, "dict", "map", object_map);
        build_dict(&mut map, "dict", "filter", object_filter);
        map
    });

    fn object_argument<'a>(
        name: &str,
        arguments: &'a [EvalValue],
    ) -> Result<&'a Arc<IndexMap<Arc<str>, EvalValue>>> {
        match arguments.first() {
            Some(EvalValue::Object(values)) => Ok(values),
            _ => bail!(Type, "{name} requires a dictionary as its first argument"),
        }
    }

    pub fn object_len(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("len", arguments, 1)?;
        Ok(EvalValue::Int(
            object_argument("len", arguments)?.len() as i64
        ))
    }

    pub fn object_indexof(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("indexof", arguments, 2)?;
        Ok(indexof_object(
            object_argument("indexof", arguments)?,
            &arguments[1],
        ))
    }

    pub fn object_keys(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("keys", arguments, 1)?;
        Ok(EvalValue::Array(
            object_argument("keys", arguments)?
                .keys()
                .cloned()
                .map(EvalValue::String)
                .collect(),
        ))
    }

    pub fn object_values(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("values", arguments, 1)?;
        Ok(EvalValue::Array(
            object_argument("values", arguments)?
                .values()
                .cloned()
                .collect(),
        ))
    }

    pub fn object_entries(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("entries", arguments, 1)?;
        Ok(EvalValue::Array(
            object_argument("entries", arguments)?
                .iter()
                .map(|(key, value)| {
                    EvalValue::Array(vec![EvalValue::String(key.clone()), value.clone()].into())
                })
                .collect(),
        ))
    }

    fn object_path<'a>(name: &str, value: &'a EvalValue) -> Result<Vec<&'a str>> {
        let EvalValue::String(path) = value else {
            bail!(Type, "{name} requires a string path")
        };
        Ok(path.split('.').collect())
    }

    pub fn object_haspath(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("haspath", arguments, 2)?;
        object_argument("haspath", arguments)?;
        Ok(EvalValue::Bool(has_path(
            &arguments[0],
            &object_path("haspath", &arguments[1])?,
        )))
    }

    pub fn object_getpath(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("getpath", arguments.len(), 2, 3)?;
        object_argument("getpath", arguments)?;
        get_path(
            &arguments[0],
            &object_path("getpath", &arguments[1])?,
            arguments.get(2).cloned(),
        )
    }

    pub fn object_tryget(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("tryget", arguments.len(), 2, 3)?;
        let values = object_argument("tryget", arguments)?;
        let EvalValue::String(key) = &arguments[1] else {
            bail!(Type, "tryget requires a string key (got: {})", &arguments[1])
        };
        Ok(values
            .get(key)
            .cloned()
            .unwrap_or_else(|| arguments.get(2).cloned().unwrap_or(EvalValue::Null)))
    }

    pub fn object_remove(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("remove", arguments, 2)?;
        let values = object_argument("remove", arguments)?;
        let EvalValue::String(key) = &arguments[1] else {
            bail!(Type, "remove requires a string key")
        };
        let mut result = values.clone();
        Arc::make_mut(&mut result).shift_remove(key);
        Ok(EvalValue::Object(result))
    }

    /// Maps dictionary values while preserving their keys. The callback receives value and key.
    pub fn object_map(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("map", arguments, 2)?;
        let values = object_argument("map", arguments)?;
        let function = arguments[1].clone();
        let mut result = IndexMap::with_capacity(values.len());
        for (key, value) in values.iter() {
            result.insert(
                key.clone(),
                function.clone().call(
                    &[value.clone(), EvalValue::String(key.clone())],
                    formula,
                    env,
                )?,
            );
        }
        Ok(EvalValue::Object(result.into()))
    }

    /// Keeps dictionary entries accepted by a callback receiving value and key.
    pub fn object_filter(
        arguments: &[EvalValue],
        formula: &mut FormulaEngine,
        env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("filter", arguments, 2)?;
        let values = object_argument("filter", arguments)?;
        let function = arguments[1].clone();
        let mut result = IndexMap::new();
        for (key, value) in values.iter() {
            if function
                .clone()
                .call(
                    &[value.clone(), EvalValue::String(key.clone())],
                    formula,
                    env,
                )?
                .truthy()
            {
                result.insert(key.clone(), value.clone());
            }
        }
        Ok(EvalValue::Object(result.into()))
    }

    //////// STRING METHODS \\\\\\\
    pub static STRING_METHODS: Lazy<IndexMap<Arc<str>, EvalValue>> = Lazy::new(|| {
        let mut map = IndexMap::new();
        build_dict(&mut map, "str", "len", string_len);
        build_dict(&mut map, "str", "indexof", string_indexof);
        build_dict(&mut map, "str", "lower", string_lower);
        build_dict(&mut map, "str", "upper", string_upper);
        build_dict(&mut map, "str", "trim", string_trim);
        build_dict(&mut map, "str", "replace", string_replace);
        build_dict(&mut map, "str", "split", string_split);
        build_dict(&mut map, "str", "startswith", string_startswith);
        build_dict(&mut map, "str", "endswith", string_endswith);
        build_dict(&mut map, "str", "capitalize", string_capitalize);
        build_dict(&mut map, "str", "title", string_title);
        build_dict(&mut map, "str", "slug", string_slug);
        build_dict(&mut map, "str", "zfill", string_zfill);
        build_dict(&mut map, "str", "fmt", string_fmt);
        build_dict(&mut map, "str", "slice", string_slice);
        map
    });

    fn string_argument<'a>(name: &str, arguments: &'a [EvalValue]) -> Result<&'a Arc<str>> {
        match arguments.first() {
            Some(EvalValue::String(value)) => Ok(value),
            _ => bail!(Type, "{name} requires a string as its first argument"),
        }
    }

    pub fn string_len(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("len", arguments, 1)?;
        Ok(EvalValue::Int(
            string_argument("len", arguments)?.chars().count() as i64,
        ))
    }

    pub fn string_indexof(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("indexof", arguments, 2)?;
        let value = string_argument("indexof", arguments)?;
        let EvalValue::String(needle) = &arguments[1] else {
            bail!(Type, "indexof requires a string argument")
        };
        Ok(indexof_str(value, needle))
    }

    pub fn string_lower(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("lower", arguments, 1)?;
        Ok(EvalValue::String(
            string_argument("lower", arguments)?.to_lowercase().into(),
        ))
    }

    pub fn string_upper(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("upper", arguments, 1)?;
        Ok(EvalValue::String(
            string_argument("upper", arguments)?.to_uppercase().into(),
        ))
    }

    pub fn string_trim(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("trim", arguments, 1)?;
        let value = string_argument("trim", arguments)?;
        let trimmed = value.trim();
        Ok(EvalValue::String(if trimmed == value.as_ref() {
            value.clone()
        } else {
            trimmed.into()
        }))
    }

    pub fn string_replace(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("replace", arguments, 3)?;
        let value = string_argument("replace", arguments)?;
        let (EvalValue::String(from), EvalValue::String(to)) = (&arguments[1], &arguments[2])
        else {
            bail!(Type, "replace requires two string arguments")
        };
        Ok(EvalValue::String(
            value.replace(from.as_ref(), to.as_ref()).into(),
        ))
    }

    pub fn string_split(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("split", arguments, 2)?;
        let value = string_argument("split", arguments)?;
        let EvalValue::String(separator) = &arguments[1] else {
            bail!(Type, "split requires a string separator")
        };
        Ok(EvalValue::Array(
            value
                .split(separator.as_ref())
                .map(|part| EvalValue::String(part.into()))
                .collect(),
        ))
    }

    pub fn string_startswith(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("startswith", arguments, 2)?;
        let value = string_argument("startswith", arguments)?;
        let EvalValue::String(prefix) = &arguments[1] else {
            bail!(Type, "startswith requires a string prefix")
        };
        Ok(EvalValue::Bool(value.starts_with(prefix.as_ref())))
    }

    pub fn string_endswith(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("endswith", arguments, 2)?;
        let value = string_argument("endswith", arguments)?;
        let EvalValue::String(suffix) = &arguments[1] else {
            bail!(Type, "endswith requires a string suffix")
        };
        Ok(EvalValue::Bool(value.ends_with(suffix.as_ref())))
    }

    pub fn string_capitalize(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("capitalize", arguments, 1)?;
        let value = string_argument("capitalize", arguments)?;
        let mut characters = value.chars();
        let result = characters.next().map_or_else(String::new, |first| {
            first.to_uppercase().collect::<String>() + characters.as_str()
        });
        Ok(EvalValue::String(result.into()))
    }

    pub fn string_title(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("title", arguments, 1)?;
        let value = string_argument("title", arguments)?;
        let result = value
            .split_whitespace()
            .map(|word| {
                let mut characters = word.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + characters.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join(" ");
        Ok(EvalValue::String(result.into()))
    }

    pub fn string_slug(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("slug", arguments, 1)?;
        let result = string_argument("slug", arguments)?
            .to_lowercase()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        Ok(EvalValue::String(result.into()))
    }

    pub fn string_zfill(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("zfill", arguments.len(), 2, 3)?;
        let value = string_argument("zfill", arguments)?;
        let width = arguments[1].to_i64()?;
        if width < 0 {
            bail!(InvalidArgument, "zfill requires a nonnegative width")
        }
        let fill = match arguments.get(2) {
            Some(EvalValue::String(fill)) if fill.chars().count() == 1 => {
                fill.chars().next().expect("one character")
            }
            Some(_) => bail!(InvalidArgument, "zfill requires a single-character fill string"),
            None => '0',
        };
        let mut result = value.to_string();
        while result.chars().count() < width as usize {
            result.insert(0, fill);
        }
        Ok(EvalValue::String(result.into()))
    }

    pub fn string_fmt(arguments: &[EvalValue], formula: &mut FormulaEngine, env: &Env) -> Result<EvalValue> {
        let Some(EvalValue::String(fmtstr)) = arguments.get(0) else {
            bail!(Type, "The first argument to fmt must be a string.");
        };
        let local_env = match arguments.get(1) {
            None => Env::new().with_parent(env),
            Some(EvalValue::Object(obj)) => Env::from_arc(obj.clone()).with_parent(env),
            Some(_) => bail!(Type, "The environment for fmt must be a dictionary."),
        };
        let res = formula.format(fmtstr.clone(), &local_env)?;
        Ok(EvalValue::String(res))
    }

    /// Returns a substring of a string, given start and end indices.
    pub fn string_slice(arguments: &[EvalValue], formula: &mut FormulaEngine, env: &Env) -> Result<EvalValue> {
        expect_arity("slice", arguments, 3)?;
        let value = string_argument("slice", arguments)?;
        let start = arguments[1].to_i64()?;
        let end = arguments[2].to_i64()?;
        let len = value.chars().count() as i64;
        let start = if start < 0 { len + start } else { start };
        let end = if end < 0 { len + end } else { end };
        if start < 0 || end < 0 || start > len || end > len || start > end {
            bail!(InvalidArgument, "slice indices are out of bounds");
        }
        let result: String = value.chars().skip(start as usize).take((end - start) as usize).collect();
        Ok(EvalValue::String(result.into()))
    }

    //////// NUMBER METHODS \\\\\\\
    pub static INT_METHODS: Lazy<IndexMap<Arc<str>, EvalValue>> = Lazy::new(|| {
        let mut map = IndexMap::new();
        build_dict(&mut map, "int", "abs", int_abs);
        build_dict(&mut map, "int", "pow", int_pow);
        build_dict(&mut map, "int", "clamp", int_clamp);
        build_dict(&mut map, "int", "sign", int_sign);
        build_dict(&mut map, "int", "lerp", int_lerp);
        build_dict(&mut map, "int", "divmod", int_divmod);
        build_dict(&mut map, "int", "tdivmod", int_tdivmod);
        build_dict(&mut map, "int", "max", max);
        build_dict(&mut map, "int", "min", min);
        build_dict(&mut map, "int", "rotr", int_rotr);
        build_dict(&mut map, "int", "rotl", int_rotl);
        map
    });
    pub static FLOAT_METHODS: Lazy<IndexMap<Arc<str>, EvalValue>> = Lazy::new(|| {
        let mut map = IndexMap::new();
        build_dict(&mut map, "float", "round", float_round);
        build_dict(&mut map, "float",  "abs", float_abs);
        build_dict(&mut map, "float", "pow", float_pow);
        build_dict(&mut map, "float", "ceil", float_ceil);
        build_dict(&mut map, "float", "floor", float_floor);
        build_dict(&mut map, "float", "clamp", float_clamp);
        build_dict(&mut map, "float", "sign", float_sign);
        build_dict(&mut map, "float", "lerp", float_lerp);
        build_dict(&mut map, "float", "max", max);
        build_dict(&mut map, "float", "min", min);
        build_dict(&mut map, "float", "bits", float_bits);
        build_dict(&mut map, "float", "isnan", float_isnan);
        build_dict(&mut map, "float", "isinf", float_isinf);
        map
    });

    fn int_argument(name: &str, arguments: &[EvalValue]) -> Result<i64> {
        match arguments.first() {
            Some(EvalValue::Int(value)) => Ok(*value),
            _ => bail!(Type, "{name} requires an integer as its first argument"),
        }
    }

    fn float_argument(name: &str, arguments: &[EvalValue]) -> Result<f64> {
        match arguments.first() {
            Some(EvalValue::Float(value)) => Ok(*value),
            _ => bail!(Type, "{name} requires a float as its first argument"),
        }
    }

    pub fn int_abs(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("abs", arguments, 1)?;
        Ok(EvalValue::Int(int_argument("abs", arguments)?.checked_abs().ok_or_else(|| Error::Overflow("Integer absolute value overflow".into()))?))
    }
    pub fn int_pow(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("pow", arguments, 2)?;
        power_values(&arguments[0], &arguments[1])
    }
    pub fn int_clamp(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("clamp", arguments, 3)?;
        let value = int_argument("clamp", arguments)?;
        let minimum = arguments[1].to_i64()?;
        let maximum = arguments[2].to_i64()?;
        if minimum > maximum {
            bail!(InvalidArgument, "clamp requires a minimum bound less than or equal to the maximum bound")
        }
        Ok(EvalValue::Int(value.clamp(minimum, maximum)))
    }
    pub fn int_sign(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("sign", arguments, 1)?;
        Ok(EvalValue::Int(int_argument("sign", arguments)?.signum()))
    }
    pub fn int_lerp(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("lerp", arguments, 3)?;
        let factor = int_argument("lerp", arguments)? as f64;
        let start = arguments[1].to_f64()?;
        let end = arguments[2].to_f64()?;
        Ok(EvalValue::Float(start + (end - start) * factor))
    }
    pub fn int_divmod(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("divmod", arguments, 2)?;
        let value = int_argument("divmod", arguments)?;
        let divisor = arguments[1].to_i64()?;
        if divisor == 0 {
            bail!(DivisionByZero, "divmod cannot divide by zero")
        }
        let mut quo = value.checked_div(divisor).ok_or_else(|| Error::Overflow("Overflow in integer divmod operation".into()))?;
        let mut rem = value.checked_rem(divisor).ok_or_else(|| Error::Overflow("Overflow in integer divmod operation".into()))?;
        if rem != 0 && (value < 0) != (divisor < 0) {
            quo -= 1;
            rem += divisor;
        }
        Ok(EvalValue::Array(
            vec![
                EvalValue::Int(quo),
                EvalValue::Int(rem),
            ]
            .into(),
        ))
    }
    /// Performs integer division and returns the quotient and remainder as `[quotient, remainder]`.
    /// The quotient is rounded toward zero (truncated).
    /// The remainder can be negative when the dividend is negative.
    pub fn int_tdivmod(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("tdivmod", arguments, 2)?;
        let value = int_argument("tdivmod", arguments)?;
        let divisor = arguments[1].to_i64()?;
        if divisor == 0 {
            bail!(DivisionByZero, "tdivmod cannot divide by zero")
        }
        let quo = value.checked_div(divisor).ok_or_else(|| Error::Overflow("Overflow in integer tdivmod operation".into()))?;
        let rem = value.checked_rem(divisor).ok_or_else(|| Error::Overflow("Overflow in integer tdivmod operation".into()))?;
        Ok(EvalValue::Array(
            vec![
                EvalValue::Int(quo),
                EvalValue::Int(rem),
            ]
            .into(),
        ))
    }
    pub fn int_rotr(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("rotr", arguments, 2)?;
        let value = int_argument("rotr", arguments)?;
        let shift = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "rotr requires an integer shift count"),
        };
        if shift < 0 {
            bail!(InvalidArgument, "rotr requires a nonnegative shift count")
        }
        let result = value.rotate_right(shift as u32);
        Ok(EvalValue::Int(result))
    }
    pub fn int_rotl(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("rotl", arguments, 2)?;
        let value = int_argument("rotl", arguments)?;
        let shift = match &arguments[1] {
            EvalValue::Int(i) => *i,
            _ => bail!(Type, "rotl requires an integer shift count"),
        };
        if shift < 0 {
            bail!(InvalidArgument, "rotl requires a nonnegative shift count")
        }
        let result = value.rotate_left(shift as u32);
        Ok(EvalValue::Int(result))
    }

    pub fn float_round(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity_between("round", arguments.len(), 1, 2)?;
        let value = float_argument("round", arguments)?;
        let digits = arguments
            .get(1)
            .map(EvalValue::to_i64)
            .transpose()?
            .unwrap_or(0);
        let rounded = python_round(value, digits)?;
        if digits <= 0 {
            Ok(EvalValue::Int(rounded as i64))
        } else {
            Ok(EvalValue::Float(rounded))
        }
    }
    pub fn float_abs(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("abs", arguments, 1)?;
        Ok(EvalValue::Float(float_argument("abs", arguments)?.abs()))
    }
    pub fn float_pow(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("pow", arguments, 2)?;
        power_values(&arguments[0], &arguments[1])
    }
    pub fn float_ceil(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("ceil", arguments, 1)?;
        Ok(EvalValue::Int(
            float_argument("ceil", arguments)?.ceil() as i64
        ))
    }
    pub fn float_floor(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("floor", arguments, 1)?;
        Ok(EvalValue::Int(
            float_argument("floor", arguments)?.floor() as i64
        ))
    }
    pub fn float_clamp(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("clamp", arguments, 3)?;
        let value = float_argument("clamp", arguments)?;
        let mut minimum = arguments[1].to_f64()?;
        let mut maximum = arguments[2].to_f64()?;
        if minimum.is_nan() || maximum.is_nan() {
            bail!(InvalidArgument, "clamp requires non-NaN bounds");
        }
        if minimum > maximum {
            std::mem::swap(&mut minimum, &mut maximum);
        }
        Ok(EvalValue::Float(value.clamp(minimum, maximum)))
    }
    pub fn float_sign(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("sign", arguments, 1)?;
        Ok(EvalValue::Int(
            float_argument("sign", arguments)?.signum() as i64
        ))
    }
    pub fn float_lerp(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("lerp", arguments, 3)?;
        let factor = float_argument("lerp", arguments)?;
        let start = arguments[1].to_f64()?;
        let end = arguments[2].to_f64()?;
        Ok(EvalValue::Float(start + (end - start) * factor))
    }
    pub fn float_bits(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("bits", arguments, 1)?;
        let value = float_argument("bits", arguments)?;
        Ok(EvalValue::Int(value.to_bits() as i64))
    }
    pub fn float_isnan(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("isnan", arguments, 1)?;
        Ok(EvalValue::Bool(float_argument("isnan", arguments)?.is_nan()))
    }
    pub fn float_isinf(
        arguments: &[EvalValue],
        _formula: &mut FormulaEngine,
        _env: &Env,
    ) -> Result<EvalValue> {
        expect_arity("isinf", arguments, 1)?;
        Ok(EvalValue::Bool(float_argument("isinf", arguments)?.is_infinite()))
    }

    pub static ALL_BUILTINS: Lazy<IndexMap<Arc<str>, EvalValue>> = Lazy::new(|| {
        let mut map = IndexMap::new();
        insert_registry(&mut map, &GLOBAL_ENV.vars);
        insert_registry(&mut map, &ARRAY_METHODS);
        insert_registry(&mut map, &OBJECT_METHODS);
        insert_registry(&mut map, &STRING_METHODS);
        insert_registry(&mut map, &INT_METHODS);
        insert_registry(&mut map, &FLOAT_METHODS);
        map
    });
}

