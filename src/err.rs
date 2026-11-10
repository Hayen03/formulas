//! Typed formula errors and their diagnostic context.
//!
//! Match on [`Error::without_context`] to handle a failure regardless of how many
//! formula or function calls it passed through. [`Error::chain`] and alternate
//! display (`format!("{error:#}")`) include the diagnostic context.
//!
//! ```
//! use formulas::{parse_next, Error};
//! use formulas::err::SyntaxError;
//!
//! let error = parse_next("1 +").unwrap_err();
//! assert!(matches!(error.without_context(), Error::Syntax {
//!     kind: SyntaxError::ExpectedExpression { .. }, ..
//! }));
//! ```

use indexmap::IndexMap;

use crate::{EvalKey, EvalValue, Token};
use std::{
    error::Error as StdError, fmt, num::{ParseFloatError, ParseIntError}, sync::Arc,
};

/// The result type used by formula parsing, evaluation, and builtin functions.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A failure to tokenize the source. Positions count Unicode scalar values.
#[derive(Debug, Clone, PartialEq)]
pub enum LexError {
    UnexpectedCharacter(char),
    LoneQuestionMark,
    MissingDigits {
        radix: u32,
    },
    InvalidDigit {
        radix: u32,
    },
    InvalidInteger {
        literal: String,
        source: ParseIntError,
    },
    InvalidFloat {
        literal: String,
        source: ParseFloatError,
    },
    IncompleteEscape,
    UnterminatedString,
    ExpectedIdentifier,
    InvalidUnicodeEscape {
        literal: String,
        source: Option<ParseIntError>,
    },
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedCharacter(character) => {
                write!(f, "Unexpected character in formula: '{character}'")
            }
            Self::LoneQuestionMark => write!(f, "A single '?' is invalid; use '??'"),
            Self::MissingDigits { radix } => {
                write!(f, "Base-{radix} literal contains no digits")
            }
            Self::InvalidDigit { radix } => {
                write!(f, "Invalid digit in base-{radix} literal")
            }
            Self::InvalidInteger { literal, .. } => {
                write!(f, "Invalid or out-of-range integer literal: {literal}")
            }
            Self::InvalidFloat { literal, .. } => {
                write!(f, "Invalid float literal: {literal}")
            }
            Self::IncompleteEscape => write!(f, "Incomplete escape sequence in string"),
            Self::UnterminatedString => write!(f, "Unterminated string in formula"),
            Self::ExpectedIdentifier => write!(f, "Expected an identifier"),
            Self::InvalidUnicodeEscape { literal, .. } => {
                write!(f, "Invalid Unicode escape sequence: {literal}")
            }
        }
    }
}

impl StdError for LexError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::InvalidInteger { source, .. } => Some(source),
            Self::InvalidFloat { source, .. } => Some(source),
            Self::InvalidUnicodeEscape { source: Some(source), .. } => Some(source),
            Self::InvalidUnicodeEscape { source: None, .. } => None,
            _ => None,
        }
    }
}

impl LexError {
    pub fn to_dict(&self) -> EvalValue {
        let mut dict = IndexMap::new();
        let mut data = IndexMap::new();
        let kind = match self {
            Self::UnexpectedCharacter(c) => {
                data.insert(EvalKey("character".into()), EvalValue::String(c.to_string().into()));
                "UnexpectedCharacter"
            },
            Self::LoneQuestionMark => "LoneQuestionMark",
            Self::MissingDigits { radix } => {
                data.insert(EvalKey("radix".into()), EvalValue::Int(*radix as i64));
                "MissingDigits"
            },
            Self::InvalidDigit { radix } => {
                data.insert(EvalKey("radix".into()), EvalValue::Int(*radix as i64));
                "InvalidDigit"
            },
            Self::InvalidInteger { literal, .. } => {
                data.insert(EvalKey("literal".into()), EvalValue::String(literal.clone().into()));
                "InvalidInteger"
            },
            Self::InvalidFloat { literal, .. } => {
                data.insert(EvalKey("literal".into()), EvalValue::String(literal.clone().into()));
                "InvalidFloat"
            },
            Self::IncompleteEscape => "IncompleteEscape",
            Self::UnterminatedString => "UnterminatedString",
            Self::ExpectedIdentifier => "ExpectedIdentifier",
            Self::InvalidUnicodeEscape { .. } => "InvalidUnicodeEscape",
        };
        dict.insert(EvalKey("kind".into()), EvalValue::String(kind.into()));
        dict.insert(EvalKey("data".into()), EvalValue::Object(Arc::new(data)));
        EvalValue::Object(Arc::new(dict))
    }
}

/// A grammar error in the token stream.
#[derive(Debug, Clone, PartialEq)]
pub enum SyntaxError {
    TrailingToken { found: Token },
    ExpectedExpression { found: Token },
    ExpectedToken { expected: Token, found: Token },
    ExpectedKeyword { expected: String, found: Token },
    ExpectedBinding { found: Token },
    ExpectedProperty { found: Token },
    InvalidBinding { name: String },
    DuplicateBinding { name: String },
    MultipleRestPatterns,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrailingToken { found } => {
                write!(f, "Unexpected token after the end of the formula: {found:?}")
            }
            Self::ExpectedExpression { found } => write!(f, "Expected an expression, got: {found:?}"),
            Self::ExpectedToken { expected, found } => {
                write!(f, "Expected token: {expected:?}, got: {found:?}")
            }
            Self::ExpectedKeyword { expected, found } => {
                write!(f, "Expected keyword: {expected}, got: {found:?}")
            }
            Self::ExpectedBinding { found } => {
                write!(f, "Expected a name or list pattern, got {found:?}")
            }
            Self::ExpectedProperty { found } => write!(
                f,
                "Expected a property name or index after '.', got: {found:?}"
            ),
            Self::InvalidBinding { name } => write!(
                f,
                "Reserved or invalid name in binding pattern: {name}"
            ),
            Self::DuplicateBinding { name } => write!(
                f,
                "Name declared twice in binding patterns: {name}"
            ),
            Self::MultipleRestPatterns => write!(
                f,
                "A list pattern can contain only one rest capture"
            ),
        }
    }
}

impl StdError for SyntaxError {}

impl SyntaxError {
    pub fn to_dict(&self) -> EvalValue {
        let mut dict = IndexMap::new();
        let mut data = IndexMap::new();
        let kind = match self {
            Self::TrailingToken { found } => {
                data.insert(EvalKey("found".into()), EvalValue::String(found.to_string().into()));
                "TrailingToken"
            },
            Self::ExpectedExpression { found } => {
                data.insert(EvalKey("found".into()), EvalValue::String(found.to_string().into()));
                "ExpectedExpression"
            },
            Self::ExpectedToken { expected, found } => {
                data.insert(EvalKey("expected".into()), EvalValue::String(expected.to_string().into()));
                data.insert(EvalKey("found".into()), EvalValue::String(found.to_string().into()));
                "ExpectedToken"
            },
            Self::ExpectedKeyword { expected, found } => {
                data.insert(EvalKey("expected".into()), EvalValue::String(expected.clone().into()));
                data.insert(EvalKey("found".into()), EvalValue::String(found.to_string().into()));
                "ExpectedKeyword"
            },
            Self::ExpectedBinding { found } => {
                data.insert(EvalKey("found".into()), EvalValue::String(found.to_string().into()));
                "ExpectedBinding"
            },
            Self::ExpectedProperty { found } => {
                data.insert(EvalKey("found".into()), EvalValue::String(found.to_string().into()));
                "ExpectedProperty"
            },
            Self::InvalidBinding { name } => {
                data.insert(EvalKey("name".into()), EvalValue::String(name.clone().into()));
                "InvalidBinding"
            },
            Self::DuplicateBinding { name } => {
                data.insert(EvalKey("name".into()), EvalValue::String(name.clone().into()));
                "DuplicateBinding"
            },
            Self::MultipleRestPatterns => "MultipleRestPatterns",
        };
        dict.insert(EvalKey("kind".into()), EvalValue::String(kind.into()));
        dict.insert(EvalKey("data".into()), EvalValue::Object(Arc::new(data)));
        EvalValue::Object(Arc::new(dict))
    }
}

/// The accepted number of arguments, including a method's receiver when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentCount {
    Exactly(usize),
    AtLeast(usize),
    Between { min: usize, max: usize },
}

impl fmt::Display for ArgumentCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exactly(count) => write!(f, "{count}"),
            Self::AtLeast(count) => write!(f, "at least {count}"),
            Self::Between { min, max } => write!(f, "{min} to {max}"),
        }
    }
}

impl ArgumentCount {
    pub fn to_dict(&self) -> EvalValue {
        let mut dict = IndexMap::new();
        let mut data = IndexMap::new();
        let kind = match self {
            Self::Exactly(count) => {
                data.insert(EvalKey("count".into()), EvalValue::Int(*count as i64));
                "Exactly"
            }
            Self::AtLeast(count) => {
                data.insert(EvalKey("count".into()), EvalValue::Int(*count as i64));
                "AtLeast"
            }
            Self::Between { min, max } => {
                data.insert(EvalKey("min".into()), EvalValue::Int(*min as i64));
                data.insert(EvalKey("max".into()), EvalValue::Int(*max as i64));
                "Between"
            }
        };
        dict.insert(EvalKey("kind".into()), EvalValue::String(kind.into()));
        dict.insert(EvalKey("data".into()), EvalValue::Object(Arc::new(data)));
        EvalValue::Object(Arc::new(dict))
    }
}

/// A formula failure. Context wrappers preserve the underlying error variant.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    Lexical {
        position: usize,
        kind: LexError,
    },
    /// `token_index` is a zero-based token offset, including expanded dice tokens.
    Syntax {
        token_index: usize,
        kind: SyntaxError,
    },
    Type(String),
    InvalidArgument(String),
    DivisionByZero(String),
    Overflow(String),
    NumericDomain(String),
    UnknownVariable {
        name: String,
        available: String,
    },
    UnknownMember {
        member: String,
        value: String,
    },
    UnknownKey {
        key: String,
    },
    /// Uses `i128` to represent both signed formula indexes and native `usize` indexes.
    IndexOutOfBounds {
        index: i128,
        length: usize,
    },
    Arity {
        function: String,
        expected: ArgumentCount,
        actual: usize,
    },
    RecursionLimit {
        limit: usize,
        call_chain: String,
    },
    InvalidBuiltin {
        name: String,
        reason: String,
    },
    InvalidInteger {
        value: String,
        source: ParseIntError,
    },
    InvalidFloat {
        value: String,
        source: ParseFloatError,
    },
    InvalidJoker,
    /// An error explicitly raised by the formula's `error` builtin.
    User(String),
    Context {
        message: String,
        source: Box<Error>,
    },
}

impl Error {
    /// Adds diagnostic context without erasing the underlying error type.
    pub fn context(self, message: impl Into<String>) -> Self {
        Self::Context {
            message: message.into(),
            source: Box::new(self),
        }
    }

    /// Returns the typed formula error beneath any diagnostic context wrappers.
    pub fn without_context(&self) -> &Self {
        let mut error = self;
        while let Self::Context { source, .. } = error {
            error = source;
        }
        error
    }

    /// Iterates over this error and all its sources, outermost first.
    pub fn chain(&self) -> impl Iterator<Item = &(dyn StdError + 'static)> {
        let mut next: Option<&(dyn StdError + 'static)> = Some(self);
        std::iter::from_fn(move || {
            let error = next?;
            next = error.source();
            Some(error)
        })
    }

    pub fn to_dict(&self) -> EvalValue {
        let source = self.without_context();
        let mut dict = IndexMap::new();
        let mut data = IndexMap::new();
        let kind = match source {
            Error::Lexical { position, kind } => {
                data.insert(EvalKey("position".into()), EvalValue::Int(*position as i64));
                data.insert(EvalKey("source".into()), kind.to_dict());
                "Lexical"
            },
            Error::Syntax { token_index, kind } => {
                data.insert(EvalKey("index".into()), EvalValue::Int(*token_index as i64));
                data.insert(EvalKey("source".into()), kind.to_dict());

                "Syntax"
            },
            Error::Type(_) => {
                //data.insert("type".into(), EvalValue::String(tp.clone().into()));
                "Type"
            },
            Error::InvalidArgument(_) => {
                //data.insert("arg".into(), EvalValue::String(arg.clone().into()));
                "InvalidArgument"
            },
            Error::DivisionByZero(_) => {
                "DivisionByZero"
            },
            Error::Overflow(_) => {
                "Overflow"
            },
            Error::NumericDomain(_) => {
                "NumericDomain"
            },
            Error::User(_) => {
                "User"
            },
            Error::UnknownVariable { name, available: _ } => {
                data.insert(EvalKey("name".into()), EvalValue::String(name.clone().into()));
                "UnknownVariable"
            },
            Error::UnknownMember { member, value } => {
                data.insert(EvalKey("member".into()), EvalValue::String(member.clone().into()));
                data.insert(EvalKey("value".into()), EvalValue::String(value.clone().into()));
                "UnknownMember"
            },
            Error::UnknownKey { key } => {
                data.insert(EvalKey("key".into()), EvalValue::String(key.clone().into()));
                "UnknownKey"
            },
            Error::IndexOutOfBounds { index, length } => {
                data.insert(EvalKey("index".into()), EvalValue::Int(*index as i64));
                data.insert(EvalKey("length".into()), EvalValue::Int(*length as i64));
                "IndexOutOfBounds"
            },
            Error::Arity { function, expected, actual } => {
                data.insert(EvalKey("function".into()), EvalValue::String(function.clone().into()));
                data.insert(EvalKey("expected".into()), expected.to_dict());
                data.insert(EvalKey("actual".into()), EvalValue::Int(*actual as i64));
                "Arity"
            },
            Error::RecursionLimit { limit, call_chain: _ } => {
                data.insert(EvalKey("limit".into()), EvalValue::Int(*limit as i64));
                // data.insert("call_chain".into(), EvalValue::Array(call_chain.iter().map(|s| EvalValue::String(s.clone().into())).collect()));
                "RecursionLimit"
            },
            Error::InvalidBuiltin { name, reason: _ } => {
                data.insert(EvalKey("name".into()), EvalValue::String(name.clone().into()));
                // data.insert("reason".into(), EvalValue::String(reason.clone().into()));
                "InvalidBuiltin"
            },
            Error::InvalidInteger { value, source: _ } => {
                data.insert(EvalKey("value".into()), EvalValue::String(value.clone().into()));
                "InvalidInteger"
            },
            Error::InvalidFloat { value, source: _ } => {
                data.insert(EvalKey("value".into()), EvalValue::String(value.clone().into()));
                "InvalidFloat"
            },
            Error::InvalidJoker => {
                "InvalidJoker"
            },
            Error::Context { message: _, source } => {
                data.insert(EvalKey("source".into()), source.to_dict());
                "Context"
            },
        };
        dict.insert(EvalKey("kind".into()), EvalValue::String(kind.into()));
        dict.insert(EvalKey("msg".into()), EvalValue::String(self.to_string().into()));
        dict.insert(EvalKey("data".into()), EvalValue::Object(Arc::from(data)));
        EvalValue::Object(Arc::from(dict))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lexical { position, kind } => write!(f, "{kind} (character {position})")?,
            Self::Syntax { token_index, kind } => write!(f, "{kind} (token {token_index})")?,
            Self::Type(message)
            | Self::InvalidArgument(message)
            | Self::DivisionByZero(message)
            | Self::Overflow(message)
            | Self::NumericDomain(message)
            | Self::User(message) => f.write_str(message)?,
            Self::UnknownVariable { name, available } => write!(
                f,
                "Unknown name `{name}`: no variable with this name is visible. Available names: {available}"
            )?,
            Self::UnknownMember { member, value } => write!(
                f,
                "Unknown member {member} on {value}: no stored value or method with this name exists"
            )?,
            Self::UnknownKey { key } => write!(f, "Unknown dictionary key: {key}")?,
            Self::IndexOutOfBounds { index, length: 0 } => {
                write!(f, "Index {index} out of bounds: the sequence is empty")?
            }
            Self::IndexOutOfBounds { index, length } => write!(
                f,
                "Index {index} out of bounds: the sequence contains {length} element(s), so valid indexes range from {} to {}",
                -(*length as i128),
                length - 1
            )?,
            Self::Arity {
                function,
                expected,
                actual,
            } => write!(
                f,
                "{function} requires {expected} argument(s), but {actual} were provided"
            )?,
            Self::RecursionLimit { limit, call_chain } => write!(
                f,
                "Maximum function call depth ({limit}) exceeded: {call_chain}"
            )?,
            Self::InvalidBuiltin { name, reason } => {
                write!(f, "Invalid builtin `{name}`: {reason}")?
            }
            Self::InvalidInteger { value, .. } => write!(
                f,
                "Cannot convert {value} to an integer: the string is not a valid integer representation"
            )?,
            Self::InvalidFloat { value, .. } => write!(
                f,
                "Cannot convert {value} to a number: the string is not a valid numeric representation"
            )?,
            Self::InvalidJoker => write!(
                f,
                "Invalid use of joker"
            )?,
            Self::Context { message, .. } => f.write_str(message)?,
        }
        if f.alternate() {
            if let Some(source) = self.source() {
                write!(f, ": {source:#}")?;
            }
        }
        Ok(())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Context { source, .. } => Some(source.as_ref()),
            Self::Lexical { kind, .. } => kind.source(),
            Self::InvalidInteger { source, .. } => Some(source),
            Self::InvalidFloat { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Internal convenience for attaching context only when an operation fails.
pub(crate) trait ResultContext<T> {
    fn context(self, message: impl Into<String>) -> Result<T>;
    fn with_context(self, message: impl FnOnce() -> String) -> Result<T>;
}

impl<T> ResultContext<T> for Result<T> {
    fn context(self, message: impl Into<String>) -> Result<T> {
        self.map_err(|error| error.context(message))
    }

    fn with_context(self, message: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|error| error.context(message()))
    }
}

// Every call site must select a concrete error variant; there is no untyped fallback.
macro_rules! error {
    ($variant:ident, $($arg:tt)*) => {
        $crate::err::Error::$variant(format!($($arg)*))
    };
}

macro_rules! bail {
    ($variant:ident, $($arg:tt)*) => {
        return Err($crate::err::error!($variant, $($arg)*))
    };
}

pub(crate) use {bail, error};
