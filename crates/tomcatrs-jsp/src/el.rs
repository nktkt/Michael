//! A self-contained **Jakarta Expression Language (EL)** evaluator.
//!
//! Jakarta EL is the `${...}` / `#{...}` expression language embedded in JSP
//! and JSF pages. Evaluating *plain* EL expressions — arithmetic, comparisons,
//! property/index navigation over a bean graph, the `empty` operator, the
//! ternary operator, and template interpolation — requires no JVM at all, so
//! Tomcat-RS implements it natively in Rust.
//!
//! This module provides:
//!
//! * [`ElValue`] — the dynamic value type, with EL's coercion rules.
//! * [`ElContext`] — variable bindings plus named scopes (`pageScope`,
//!   `requestScope`, `sessionScope`, `applicationScope`) and a registry of
//!   host-provided functions.
//! * [`Expression`] — a parsed expression: [`Expression::parse`] accepts either
//!   a `${...}` / `#{...}` wrapper or a bare expression body, and
//!   [`Expression::evaluate`] runs it against an [`ElContext`].
//! * [`interpolate`] — substitutes every `${...}` / `#{...}` fragment found in a
//!   template string with its evaluated result.
//! * [`ElError`] — a precise error type that converts into
//!   [`tomcatrs_core::Error`].
//!
//! The parser is a hand-rolled tokenizer plus recursive-descent parser; it
//! pulls in no external crates.
//!
//! # Example
//!
//! ```
//! use tomcatrs_jsp::el::{ElContext, ElValue, Expression, interpolate};
//!
//! let mut ctx = ElContext::new();
//! ctx.set_variable("x", ElValue::Long(10));
//!
//! let expr = Expression::parse("${x * 2 + 1}").unwrap();
//! assert_eq!(expr.evaluate(&ctx).unwrap(), ElValue::Long(21));
//!
//! assert_eq!(interpolate("x=${x}!", &ctx).unwrap(), "x=10!");
//! ```

use std::collections::BTreeMap;
use std::fmt;

use tomcatrs_core::Error as CoreError;

// ===========================================================================
// Errors
// ===========================================================================

/// An error raised while tokenizing, parsing, or evaluating an EL expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElError {
    /// The tokenizer hit a character or sequence it could not classify.
    Lex(String),
    /// The parser found a token that is not valid at the current position.
    Parse(String),
    /// Evaluation failed (unknown identifier, bad coercion, divide by zero…).
    Eval(String),
}

impl fmt::Display for ElError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElError::Lex(m) => write!(f, "EL lex error: {m}"),
            ElError::Parse(m) => write!(f, "EL parse error: {m}"),
            ElError::Eval(m) => write!(f, "EL evaluation error: {m}"),
        }
    }
}

impl std::error::Error for ElError {}

impl From<ElError> for CoreError {
    fn from(e: ElError) -> Self {
        CoreError::Other(e.to_string())
    }
}

/// `Result` alias for fallible EL operations.
pub type ElResult<T> = std::result::Result<T, ElError>;

// ===========================================================================
// Values
// ===========================================================================

/// The dynamic value type produced by EL evaluation.
///
/// EL is dynamically typed; every sub-expression yields one of these variants.
/// Coercion between variants follows the Jakarta EL specification closely
/// enough for the common JSP/JSF subset (see [`ElValue::as_number`],
/// [`ElValue::coerce_to_string`], and [`ElValue::is_truthy`]).
#[derive(Debug, Clone)]
pub enum ElValue {
    /// The EL `null` literal, and the result of resolving a missing property.
    Null,
    /// A boolean (`true` / `false`).
    Bool(bool),
    /// An integral number. Integer literals and integer arithmetic stay here.
    Long(i64),
    /// A floating-point number. Float literals and any `/` division land here.
    Double(f64),
    /// A text string.
    String(String),
    /// An ordered list, indexable with `[i]`.
    List(Vec<ElValue>),
    /// A string-keyed map, navigable with `.key` or `["key"]`.
    Map(BTreeMap<String, ElValue>),
}

impl ElValue {
    /// Returns the EL "truthiness" of this value, used by `&&`, `||`, `!`,
    /// and the ternary operator's condition.
    ///
    /// `null` is false, booleans are themselves, the empty string and the
    /// string `"false"` are false (all other strings true), `0` numbers are
    /// false, and empty collections are false.
    pub fn is_truthy(&self) -> bool {
        match self {
            ElValue::Null => false,
            ElValue::Bool(b) => *b,
            ElValue::Long(n) => *n != 0,
            ElValue::Double(d) => *d != 0.0,
            ElValue::String(s) => !s.is_empty() && !s.eq_ignore_ascii_case("false"),
            ElValue::List(v) => !v.is_empty(),
            ElValue::Map(m) => !m.is_empty(),
        }
    }

    /// Implements the EL `empty` operator: `null`, the empty string, an empty
    /// list, and an empty map are all "empty"; everything else is not.
    pub fn is_empty(&self) -> bool {
        match self {
            ElValue::Null => true,
            ElValue::String(s) => s.is_empty(),
            ElValue::List(v) => v.is_empty(),
            ElValue::Map(m) => m.is_empty(),
            _ => false,
        }
    }

    /// Coerces this value to a Rust [`String`] following EL rules: `null`
    /// becomes the empty string, numbers and booleans use their natural
    /// rendering, and collections use their [`Display`] form.
    pub fn coerce_to_string(&self) -> String {
        match self {
            ElValue::Null => String::new(),
            other => other.to_string(),
        }
    }

    /// Coerces this value to a boolean. Strings are parsed case-insensitively
    /// (`"true"` → `true`, anything else → `false`); other variants reuse
    /// [`is_truthy`](ElValue::is_truthy).
    pub fn coerce_to_bool(&self) -> bool {
        match self {
            ElValue::Bool(b) => *b,
            ElValue::String(s) => s.eq_ignore_ascii_case("true"),
            other => other.is_truthy(),
        }
    }

    /// Coerces this value to a number for arithmetic.
    ///
    /// The result is `Long` when the value is integral and `Double` when it is
    /// fractional. `null` coerces to `Long(0)`. Strings are parsed as either an
    /// integer or a float. Returns an [`ElError::Eval`] when a string cannot be
    /// parsed or the variant has no numeric meaning.
    pub fn as_number(&self) -> ElResult<ElValue> {
        match self {
            ElValue::Null => Ok(ElValue::Long(0)),
            ElValue::Bool(_) => Err(ElError::Eval("cannot coerce boolean to number".into())),
            ElValue::Long(_) | ElValue::Double(_) => Ok(self.clone()),
            ElValue::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Ok(ElValue::Long(0));
                }
                if let Ok(i) = t.parse::<i64>() {
                    Ok(ElValue::Long(i))
                } else if let Ok(d) = t.parse::<f64>() {
                    Ok(ElValue::Double(d))
                } else {
                    Err(ElError::Eval(format!(
                        "cannot coerce string {s:?} to number"
                    )))
                }
            }
            ElValue::List(_) | ElValue::Map(_) => {
                Err(ElError::Eval("cannot coerce collection to number".into()))
            }
        }
    }

    /// Coerces this value to an `f64`, used when either operand of an
    /// arithmetic or relational operator is fractional.
    pub fn as_f64(&self) -> ElResult<f64> {
        match self.as_number()? {
            ElValue::Long(i) => Ok(i as f64),
            ElValue::Double(d) => Ok(d),
            _ => unreachable!("as_number only yields Long or Double"),
        }
    }

    /// Coerces this value to an `i64`. A fractional `Double` is truncated
    /// toward zero, matching EL's narrowing behaviour for index expressions.
    pub fn as_i64(&self) -> ElResult<i64> {
        match self.as_number()? {
            ElValue::Long(i) => Ok(i),
            ElValue::Double(d) => Ok(d as i64),
            _ => unreachable!("as_number only yields Long or Double"),
        }
    }

    /// Short type name, used in error messages.
    fn type_name(&self) -> &'static str {
        match self {
            ElValue::Null => "null",
            ElValue::Bool(_) => "boolean",
            ElValue::Long(_) => "long",
            ElValue::Double(_) => "double",
            ElValue::String(_) => "string",
            ElValue::List(_) => "list",
            ElValue::Map(_) => "map",
        }
    }
}

impl fmt::Display for ElValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElValue::Null => write!(f, ""),
            ElValue::Bool(b) => write!(f, "{b}"),
            ElValue::Long(n) => write!(f, "{n}"),
            ElValue::Double(d) => {
                // Render integral doubles without a trailing ".0" only when
                // they are not integral; EL's `String` coercion keeps the
                // fractional form otherwise. We mirror Java's `Double.toString`
                // loosely: integral doubles still show ".0".
                if d.fract() == 0.0 && d.is_finite() {
                    write!(f, "{d:.1}")
                } else {
                    write!(f, "{d}")
                }
            }
            ElValue::String(s) => write!(f, "{s}"),
            ElValue::List(v) => {
                write!(f, "[")?;
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            ElValue::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, val)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}={val}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// Equality for [`ElValue`] follows EL's `==` semantics: numbers compare by
/// numeric value across `Long`/`Double`, and `null` equals only `null`.
impl PartialEq for ElValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ElValue::Null, ElValue::Null) => true,
            (ElValue::Null, _) | (_, ElValue::Null) => false,
            (ElValue::Bool(a), ElValue::Bool(b)) => a == b,
            (ElValue::String(a), ElValue::String(b)) => a == b,
            (ElValue::List(a), ElValue::List(b)) => a == b,
            (ElValue::Map(a), ElValue::Map(b)) => a == b,
            // Numeric cross-comparison.
            (ElValue::Long(a), ElValue::Long(b)) => a == b,
            (ElValue::Long(_), ElValue::Double(_))
            | (ElValue::Double(_), ElValue::Long(_))
            | (ElValue::Double(_), ElValue::Double(_)) => match (self.as_f64(), other.as_f64()) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            },
            _ => false,
        }
    }
}

// ===========================================================================
// Context
// ===========================================================================

/// A host-registered EL function: takes the evaluated argument list and
/// returns a value or an error.
pub type ElFunction = fn(&[ElValue]) -> ElResult<ElValue>;

/// Variable bindings, named scopes, and registered functions for evaluation.
///
/// Plain identifiers are resolved first against the top-level [`set_variable`]
/// bindings, then — failing that — by searching every named scope in
/// registration order. The named scopes also surface as identifiers
/// themselves, so `${sessionScope.user}` resolves the `sessionScope` map and
/// then navigates into it.
///
/// [`set_variable`]: ElContext::set_variable
#[derive(Default)]
pub struct ElContext {
    variables: BTreeMap<String, ElValue>,
    scopes: BTreeMap<String, BTreeMap<String, ElValue>>,
    functions: BTreeMap<String, ElFunction>,
}

impl ElContext {
    /// Creates an empty context with no variables, scopes, or functions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds a top-level variable. Top-level variables shadow scope entries
    /// of the same name.
    pub fn set_variable(&mut self, name: impl Into<String>, value: ElValue) {
        self.variables.insert(name.into(), value);
    }

    /// Installs (or replaces) a named scope such as `"sessionScope"`.
    ///
    /// The scope map becomes reachable both as a navigable map under its own
    /// name and as a fallback lookup for bare identifiers.
    pub fn set_scope(&mut self, name: impl Into<String>, entries: BTreeMap<String, ElValue>) {
        self.scopes.insert(name.into(), entries);
    }

    /// Registers a host function callable from EL as `name(args...)` (or, if
    /// `name` contains a colon, as `prefix:local(args...)`).
    pub fn register_function(&mut self, name: impl Into<String>, func: ElFunction) {
        self.functions.insert(name.into(), func);
    }

    /// Resolves a bare identifier to a value, searching top-level variables,
    /// then scope maps surfaced as values, then scope contents.
    fn resolve(&self, name: &str) -> Option<ElValue> {
        if let Some(v) = self.variables.get(name) {
            return Some(v.clone());
        }
        if let Some(scope) = self.scopes.get(name) {
            return Some(ElValue::Map(scope.clone()));
        }
        for scope in self.scopes.values() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    /// Looks up a registered function by name.
    fn function(&self, name: &str) -> Option<ElFunction> {
        self.functions.get(name).copied()
    }
}

// ===========================================================================
// Tokenizer
// ===========================================================================

/// A lexical token produced by [`tokenize`].
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// An integer literal.
    Long(i64),
    /// A floating-point literal.
    Double(f64),
    /// A string literal (quotes stripped, escapes resolved).
    Str(String),
    /// `true` / `false`.
    Bool(bool),
    /// `null`.
    Null,
    /// An identifier (or keyword that the parser treats as one contextually).
    Ident(String),
    /// A punctuator or operator: `+ - * / % ( ) [ ] . , ? :` plus the
    /// multi-character comparison/logical operators stored verbatim.
    Op(&'static str),
}

/// Tokenizes an EL expression body into a flat token list.
fn tokenize(src: &str) -> ElResult<Vec<Token>> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();

    while i < chars.len() {
        let c = chars[i];

        // Whitespace.
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Numbers: integer or float.
        if c.is_ascii_digit() || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            let mut seen_dot = false;
            let mut seen_exp = false;
            while i < chars.len() {
                let d = chars[i];
                if d.is_ascii_digit() {
                    i += 1;
                } else if d == '.' && !seen_dot && !seen_exp {
                    seen_dot = true;
                    i += 1;
                } else if (d == 'e' || d == 'E') && !seen_exp {
                    seen_exp = true;
                    i += 1;
                    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                        i += 1;
                    }
                } else {
                    break;
                }
            }
            let text: String = chars[start..i].iter().collect();
            if seen_dot || seen_exp {
                let d = text
                    .parse::<f64>()
                    .map_err(|e| ElError::Lex(format!("bad float literal {text:?}: {e}")))?;
                out.push(Token::Double(d));
            } else {
                let n = text
                    .parse::<i64>()
                    .map_err(|e| ElError::Lex(format!("bad int literal {text:?}: {e}")))?;
                out.push(Token::Long(n));
            }
            continue;
        }

        // String literals: single or double quoted, with `\` escapes.
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            let mut closed = false;
            while i < chars.len() {
                let d = chars[i];
                if d == '\\' && i + 1 < chars.len() {
                    let esc = chars[i + 1];
                    s.push(match esc {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        other => other,
                    });
                    i += 2;
                    continue;
                }
                if d == quote {
                    closed = true;
                    i += 1;
                    break;
                }
                s.push(d);
                i += 1;
            }
            if !closed {
                return Err(ElError::Lex(format!(
                    "unterminated string literal starting with {quote}"
                )));
            }
            out.push(Token::Str(s));
            continue;
        }

        // Identifiers and keywords.
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '$'
                    || chars[i] == ':')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "true" => out.push(Token::Bool(true)),
                "false" => out.push(Token::Bool(false)),
                "null" => out.push(Token::Null),
                "div" => out.push(Token::Op("/")),
                "mod" => out.push(Token::Op("%")),
                "and" => out.push(Token::Op("&&")),
                "or" => out.push(Token::Op("||")),
                "not" => out.push(Token::Op("!")),
                "eq" => out.push(Token::Op("==")),
                "ne" => out.push(Token::Op("!=")),
                "lt" => out.push(Token::Op("<")),
                "gt" => out.push(Token::Op(">")),
                "le" => out.push(Token::Op("<=")),
                "ge" => out.push(Token::Op(">=")),
                "empty" => out.push(Token::Op("empty")),
                _ => out.push(Token::Ident(word)),
            }
            continue;
        }

        // Multi- and single-character operators.
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let op2: Option<&'static str> = match two.as_str() {
            "==" => Some("=="),
            "!=" => Some("!="),
            "<=" => Some("<="),
            ">=" => Some(">="),
            "&&" => Some("&&"),
            "||" => Some("||"),
            _ => None,
        };
        if let Some(op) = op2 {
            out.push(Token::Op(op));
            i += 2;
            continue;
        }

        let op1: Option<&'static str> = match c {
            '+' => Some("+"),
            '-' => Some("-"),
            '*' => Some("*"),
            '/' => Some("/"),
            '%' => Some("%"),
            '(' => Some("("),
            ')' => Some(")"),
            '[' => Some("["),
            ']' => Some("]"),
            '.' => Some("."),
            ',' => Some(","),
            '?' => Some("?"),
            ':' => Some(":"),
            '!' => Some("!"),
            '<' => Some("<"),
            '>' => Some(">"),
            _ => None,
        };
        if let Some(op) = op1 {
            out.push(Token::Op(op));
            i += 1;
            continue;
        }

        return Err(ElError::Lex(format!("unexpected character {c:?}")));
    }

    Ok(out)
}

// ===========================================================================
// AST
// ===========================================================================

/// A node in the parsed expression tree.
#[derive(Debug, Clone, PartialEq)]
enum Ast {
    /// A constant literal value.
    Literal(LiteralAst),
    /// A bare identifier reference, resolved against the [`ElContext`].
    Ident(String),
    /// `.name` property access on a sub-expression.
    Property(Box<Ast>, String),
    /// `[index]` access on a sub-expression (list index or map key).
    Index(Box<Ast>, Box<Ast>),
    /// A unary operator (`-`, `!`/`not`, `empty`) applied to one operand.
    Unary(&'static str, Box<Ast>),
    /// A binary operator applied to two operands.
    Binary(&'static str, Box<Ast>, Box<Ast>),
    /// `cond ? then : otherwise`.
    Ternary(Box<Ast>, Box<Ast>, Box<Ast>),
    /// A call to a registered function: `name(args...)`.
    Call(String, Vec<Ast>),
}

/// A literal value captured at parse time. Kept separate from [`ElValue`] so
/// the AST stays `PartialEq` without depending on `ElValue`'s custom equality.
#[derive(Debug, Clone, PartialEq)]
enum LiteralAst {
    /// `null`.
    Null,
    /// A boolean literal.
    Bool(bool),
    /// An integer literal.
    Long(i64),
    /// A float literal, stored as bits so the enum can derive `PartialEq`.
    Double(u64),
    /// A string literal.
    Str(String),
}

impl LiteralAst {
    /// Materializes this literal as a runtime [`ElValue`].
    fn to_value(&self) -> ElValue {
        match self {
            LiteralAst::Null => ElValue::Null,
            LiteralAst::Bool(b) => ElValue::Bool(*b),
            LiteralAst::Long(n) => ElValue::Long(*n),
            LiteralAst::Double(bits) => ElValue::Double(f64::from_bits(*bits)),
            LiteralAst::Str(s) => ElValue::String(s.clone()),
        }
    }
}

// ===========================================================================
// Parser
// ===========================================================================

/// A recursive-descent parser over a [`Token`] slice.
///
/// Precedence (loosest to tightest): ternary → `||` → `&&` → equality →
/// relational → additive → multiplicative → unary → postfix (`.`/`[]`/call) →
/// primary.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    /// Creates a parser positioned at the first token.
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Returns the current token without consuming it.
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    /// Consumes and returns the current token.
    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// Returns `true` and consumes the token if it is the given operator.
    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Token::Op(o)) if *o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes the given operator or returns an [`ElError::Parse`].
    fn expect_op(&mut self, op: &str) -> ElResult<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(ElError::Parse(format!(
                "expected {op:?}, found {:?}",
                self.peek()
            )))
        }
    }

    /// Parses a complete expression and asserts that all tokens were consumed.
    fn parse_full(&mut self) -> ElResult<Ast> {
        if self.tokens.is_empty() {
            return Err(ElError::Parse("empty expression".into()));
        }
        let ast = self.parse_ternary()?;
        if self.pos != self.tokens.len() {
            return Err(ElError::Parse(format!(
                "unexpected trailing token {:?}",
                self.peek()
            )));
        }
        Ok(ast)
    }

    /// `ternary := or ('?' ternary ':' ternary)?`
    fn parse_ternary(&mut self) -> ElResult<Ast> {
        let cond = self.parse_or()?;
        if self.eat_op("?") {
            let then = self.parse_ternary()?;
            self.expect_op(":")?;
            let otherwise = self.parse_ternary()?;
            Ok(Ast::Ternary(
                Box::new(cond),
                Box::new(then),
                Box::new(otherwise),
            ))
        } else {
            Ok(cond)
        }
    }

    /// `or := and ('||' and)*`
    fn parse_or(&mut self) -> ElResult<Ast> {
        let mut left = self.parse_and()?;
        while self.eat_op("||") {
            let right = self.parse_and()?;
            left = Ast::Binary("||", Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `and := equality ('&&' equality)*`
    fn parse_and(&mut self) -> ElResult<Ast> {
        let mut left = self.parse_equality()?;
        while self.eat_op("&&") {
            let right = self.parse_equality()?;
            left = Ast::Binary("&&", Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `equality := relational (('=='|'!=') relational)*`
    fn parse_equality(&mut self) -> ElResult<Ast> {
        let mut left = self.parse_relational()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op(o @ ("==" | "!="))) => *o,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_relational()?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `relational := additive (('<'|'>'|'<='|'>=') additive)*`
    fn parse_relational(&mut self) -> ElResult<Ast> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op(o @ ("<" | ">" | "<=" | ">="))) => *o,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_additive()?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `additive := multiplicative (('+'|'-') multiplicative)*`
    fn parse_additive(&mut self) -> ElResult<Ast> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op(o @ ("+" | "-"))) => *o,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_multiplicative()?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `multiplicative := unary (('*'|'/'|'%') unary)*`
    fn parse_multiplicative(&mut self) -> ElResult<Ast> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op(o @ ("*" | "/" | "%"))) => *o,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_unary()?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `unary := ('-'|'!'|'empty') unary | postfix`
    fn parse_unary(&mut self) -> ElResult<Ast> {
        for op in ["-", "!", "empty"] {
            if self.eat_op(op) {
                let operand = self.parse_unary()?;
                return Ok(Ast::Unary(
                    match op {
                        "-" => "-",
                        "!" => "!",
                        _ => "empty",
                    },
                    Box::new(operand),
                ));
            }
        }
        self.parse_postfix()
    }

    /// `postfix := primary ('.' ident | '[' expr ']')*`
    fn parse_postfix(&mut self) -> ElResult<Ast> {
        let mut node = self.parse_primary()?;
        loop {
            if self.eat_op(".") {
                match self.next() {
                    Some(Token::Ident(name)) => {
                        node = Ast::Property(Box::new(node), name);
                    }
                    other => {
                        return Err(ElError::Parse(format!(
                            "expected property name after '.', found {other:?}"
                        )));
                    }
                }
            } else if self.eat_op("[") {
                let index = self.parse_ternary()?;
                self.expect_op("]")?;
                node = Ast::Index(Box::new(node), Box::new(index));
            } else {
                break;
            }
        }
        Ok(node)
    }

    /// `primary := literal | '(' expr ')' | ident | ident '(' args ')'`
    fn parse_primary(&mut self) -> ElResult<Ast> {
        match self.next() {
            Some(Token::Long(n)) => Ok(Ast::Literal(LiteralAst::Long(n))),
            Some(Token::Double(d)) => Ok(Ast::Literal(LiteralAst::Double(d.to_bits()))),
            Some(Token::Str(s)) => Ok(Ast::Literal(LiteralAst::Str(s))),
            Some(Token::Bool(b)) => Ok(Ast::Literal(LiteralAst::Bool(b))),
            Some(Token::Null) => Ok(Ast::Literal(LiteralAst::Null)),
            Some(Token::Op("(")) => {
                let inner = self.parse_ternary()?;
                self.expect_op(")")?;
                Ok(inner)
            }
            Some(Token::Ident(name)) => {
                // Function call?
                if self.eat_op("(") {
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Token::Op(")"))) {
                        loop {
                            args.push(self.parse_ternary()?);
                            if self.eat_op(",") {
                                continue;
                            }
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    Ok(Ast::Call(name, args))
                } else {
                    Ok(Ast::Ident(name))
                }
            }
            other => Err(ElError::Parse(format!(
                "unexpected token in primary position: {other:?}"
            ))),
        }
    }
}

// ===========================================================================
// Expression
// ===========================================================================

/// A parsed, reusable EL expression.
///
/// Construct one with [`Expression::parse`] and run it any number of times
/// against different [`ElContext`]s with [`Expression::evaluate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    ast: Ast,
    /// The original source body (the part inside `${...}`), kept for
    /// diagnostics and [`Display`].
    source: String,
}

impl Expression {
    /// Parses an EL expression.
    ///
    /// The input may be wrapped in `${...}` or `#{...}`, or it may be a bare
    /// expression body — both forms are accepted. Surrounding whitespace is
    /// ignored. Returns an [`ElError`] on a lexical or syntactic failure.
    pub fn parse(input: &str) -> ElResult<Expression> {
        let body = strip_wrapper(input.trim()).unwrap_or(input.trim());
        let tokens = tokenize(body)?;
        let ast = Parser::new(tokens).parse_full()?;
        Ok(Expression {
            ast,
            source: body.to_string(),
        })
    }

    /// Evaluates the expression against `ctx`, producing an [`ElValue`].
    pub fn evaluate(&self, ctx: &ElContext) -> ElResult<ElValue> {
        eval(&self.ast, ctx)
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${{{}}}", self.source)
    }
}

/// Strips a single `${...}` or `#{...}` wrapper, returning the inner body.
/// Returns `None` when `input` is not wrapped (the caller then treats the
/// whole string as a bare body).
fn strip_wrapper(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    if bytes.len() >= 3
        && (bytes[0] == b'$' || bytes[0] == b'#')
        && bytes[1] == b'{'
        && bytes[bytes.len() - 1] == b'}'
    {
        Some(&input[2..input.len() - 1])
    } else {
        None
    }
}

// ===========================================================================
// Evaluation
// ===========================================================================

/// Recursively evaluates an AST node.
fn eval(node: &Ast, ctx: &ElContext) -> ElResult<ElValue> {
    match node {
        Ast::Literal(lit) => Ok(lit.to_value()),

        Ast::Ident(name) => Ok(ctx.resolve(name).unwrap_or(ElValue::Null)),

        Ast::Property(base, name) => {
            let base_val = eval(base, ctx)?;
            get_member(&base_val, &ElValue::String(name.clone()))
        }

        Ast::Index(base, index) => {
            let base_val = eval(base, ctx)?;
            let index_val = eval(index, ctx)?;
            get_member(&base_val, &index_val)
        }

        Ast::Unary(op, operand) => {
            let v = eval(operand, ctx)?;
            match *op {
                "-" => match v.as_number()? {
                    ElValue::Long(n) => Ok(ElValue::Long(-n)),
                    ElValue::Double(d) => Ok(ElValue::Double(-d)),
                    _ => unreachable!(),
                },
                "!" => Ok(ElValue::Bool(!v.is_truthy())),
                "empty" => Ok(ElValue::Bool(v.is_empty())),
                _ => Err(ElError::Eval(format!("unknown unary operator {op}"))),
            }
        }

        Ast::Binary(op, left, right) => eval_binary(op, left, right, ctx),

        Ast::Ternary(cond, then, otherwise) => {
            if eval(cond, ctx)?.is_truthy() {
                eval(then, ctx)
            } else {
                eval(otherwise, ctx)
            }
        }

        Ast::Call(name, args) => {
            let func = ctx
                .function(name)
                .ok_or_else(|| ElError::Eval(format!("unknown function {name:?}")))?;
            let mut argv = Vec::with_capacity(args.len());
            for a in args {
                argv.push(eval(a, ctx)?);
            }
            func(&argv)
        }
    }
}

/// Evaluates a binary operator, applying short-circuit semantics for the
/// logical operators.
fn eval_binary(op: &str, left: &Ast, right: &Ast, ctx: &ElContext) -> ElResult<ElValue> {
    // Short-circuiting logical operators.
    match op {
        "&&" => {
            let l = eval(left, ctx)?;
            if !l.is_truthy() {
                return Ok(ElValue::Bool(false));
            }
            return Ok(ElValue::Bool(eval(right, ctx)?.is_truthy()));
        }
        "||" => {
            let l = eval(left, ctx)?;
            if l.is_truthy() {
                return Ok(ElValue::Bool(true));
            }
            return Ok(ElValue::Bool(eval(right, ctx)?.is_truthy()));
        }
        _ => {}
    }

    let l = eval(left, ctx)?;
    let r = eval(right, ctx)?;

    match op {
        "+" | "-" | "*" | "/" | "%" => arithmetic(op, &l, &r),

        "==" => Ok(ElValue::Bool(values_equal(&l, &r))),
        "!=" => Ok(ElValue::Bool(!values_equal(&l, &r))),

        "<" | ">" | "<=" | ">=" => {
            let ord = compare(&l, &r)?;
            let result = match op {
                "<" => ord == std::cmp::Ordering::Less,
                ">" => ord == std::cmp::Ordering::Greater,
                "<=" => ord != std::cmp::Ordering::Greater,
                ">=" => ord != std::cmp::Ordering::Less,
                _ => unreachable!(),
            };
            Ok(ElValue::Bool(result))
        }

        _ => Err(ElError::Eval(format!("unknown binary operator {op}"))),
    }
}

/// Implements EL `==` / `!=` equality, including the string-concatenation-free
/// numeric cross-comparison handled by [`ElValue`]'s `PartialEq`.
fn values_equal(l: &ElValue, r: &ElValue) -> bool {
    // If either side is numeric and the other is a numeric-looking string,
    // EL coerces and compares numerically.
    match (l, r) {
        (ElValue::Long(_) | ElValue::Double(_), ElValue::String(_))
        | (ElValue::String(_), ElValue::Long(_) | ElValue::Double(_)) => {
            match (l.as_number(), r.as_number()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        }
        _ => l == r,
    }
}

/// Applies an arithmetic operator with EL coercion: `+ - * %` stay integral
/// when both operands are integral, while `/` is always floating-point.
///
/// The one EL-specific wrinkle handled here: `+` is *purely numeric* in
/// Jakarta EL (string concatenation uses no operator), so `"3" + 4` yields the
/// number `7`, not the string `"34"`.
fn arithmetic(op: &str, l: &ElValue, r: &ElValue) -> ElResult<ElValue> {
    let ln = l.as_number()?;
    let rn = r.as_number()?;

    let both_long = matches!(ln, ElValue::Long(_)) && matches!(rn, ElValue::Long(_));

    if op == "/" {
        // EL division is always floating point.
        let a = ln.as_f64()?;
        let b = rn.as_f64()?;
        return Ok(ElValue::Double(a / b));
    }

    if both_long {
        let a = ln.as_i64()?;
        let b = rn.as_i64()?;
        let result = match op {
            "+" => a.wrapping_add(b),
            "-" => a.wrapping_sub(b),
            "*" => a.wrapping_mul(b),
            "%" => {
                if b == 0 {
                    return Err(ElError::Eval("integer modulo by zero".into()));
                }
                a % b
            }
            _ => unreachable!(),
        };
        Ok(ElValue::Long(result))
    } else {
        let a = ln.as_f64()?;
        let b = rn.as_f64()?;
        let result = match op {
            "+" => a + b,
            "-" => a - b,
            "*" => a * b,
            "%" => a % b,
            _ => unreachable!(),
        };
        Ok(ElValue::Double(result))
    }
}

/// Compares two values for the relational operators.
///
/// Numbers compare numerically; strings compare lexicographically; mixed
/// number/string pairs coerce both sides to numbers. Other combinations are an
/// [`ElError::Eval`].
fn compare(l: &ElValue, r: &ElValue) -> ElResult<std::cmp::Ordering> {
    match (l, r) {
        (ElValue::String(a), ElValue::String(b)) => Ok(a.cmp(b)),
        (ElValue::Null, _) | (_, ElValue::Null) => Err(ElError::Eval(
            "cannot apply relational operator to null".into(),
        )),
        _ => {
            // Numeric (with string coercion) comparison.
            let a = l.as_f64()?;
            let b = r.as_f64()?;
            a.partial_cmp(&b)
                .ok_or_else(|| ElError::Eval("cannot order NaN values".into()))
        }
    }
}

/// Resolves `base.member` / `base[member]` access.
///
/// * On a [`ElValue::Map`], `member` is coerced to a string key.
/// * On a [`ElValue::List`], `member` is coerced to an integer index; an
///   out-of-range index yields [`ElValue::Null`].
/// * On [`ElValue::Null`], the result is [`ElValue::Null`] (EL is lenient about
///   navigating through missing values).
fn get_member(base: &ElValue, member: &ElValue) -> ElResult<ElValue> {
    match base {
        ElValue::Null => Ok(ElValue::Null),
        ElValue::Map(m) => {
            let key = member.coerce_to_string();
            Ok(m.get(&key).cloned().unwrap_or(ElValue::Null))
        }
        ElValue::List(v) => {
            let idx = member.as_i64()?;
            if idx < 0 || idx as usize >= v.len() {
                Ok(ElValue::Null)
            } else {
                Ok(v[idx as usize].clone())
            }
        }
        ElValue::String(s) => {
            // Allow indexing into a string by character position.
            let idx = member.as_i64()?;
            if idx < 0 {
                return Ok(ElValue::Null);
            }
            match s.chars().nth(idx as usize) {
                Some(c) => Ok(ElValue::String(c.to_string())),
                None => Ok(ElValue::Null),
            }
        }
        other => Err(ElError::Eval(format!(
            "cannot access member of {} value",
            other.type_name()
        ))),
    }
}

// ===========================================================================
// Template interpolation
// ===========================================================================

/// Substitutes every `${...}` and `#{...}` fragment in `template` with its
/// evaluated, string-coerced result.
///
/// Text outside the fragments is copied verbatim. A literal dollar or hash that
/// is not followed by `{` is also copied verbatim. This mirrors how EL is
/// applied to JSP template text.
///
/// # Errors
///
/// Returns the first [`ElError`] encountered while parsing or evaluating any
/// fragment, or an error if a fragment is left unterminated (missing `}`).
pub fn interpolate(template: &str, ctx: &ElContext) -> ElResult<String> {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if (c == '$' || c == '#') && i + 1 < chars.len() && chars[i + 1] == '{' {
            // Find the matching '}', respecting nested braces and string
            // literals so `${m['a}b']}` works.
            let mut depth = 1;
            let mut j = i + 2;
            let mut in_str: Option<char> = None;
            let mut body = String::new();
            while j < chars.len() {
                let d = chars[j];
                if let Some(q) = in_str {
                    body.push(d);
                    if d == '\\' && j + 1 < chars.len() {
                        body.push(chars[j + 1]);
                        j += 2;
                        continue;
                    }
                    if d == q {
                        in_str = None;
                    }
                    j += 1;
                    continue;
                }
                match d {
                    '\'' | '"' => {
                        in_str = Some(d);
                        body.push(d);
                    }
                    '{' => {
                        depth += 1;
                        body.push(d);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(d);
                    }
                    _ => body.push(d),
                }
                j += 1;
            }
            if depth != 0 {
                return Err(ElError::Parse(format!(
                    "unterminated EL fragment starting at byte offset {i}"
                )));
            }
            let expr = Expression::parse(&body)?;
            let value = expr.evaluate(ctx)?;
            out.push_str(&value.coerce_to_string());
            i = j + 1; // skip past the closing '}'
        } else {
            out.push(c);
            i += 1;
        }
    }

    Ok(out)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses and evaluates `src` against `ctx`, panicking on any error.
    fn eval_str(src: &str, ctx: &ElContext) -> ElValue {
        Expression::parse(src)
            .expect("parse")
            .evaluate(ctx)
            .expect("evaluate")
    }

    /// Evaluates against an empty context.
    fn eval_empty(src: &str) -> ElValue {
        eval_str(src, &ElContext::new())
    }

    // ---- literals -------------------------------------------------------

    #[test]
    fn literals() {
        assert_eq!(eval_empty("42"), ElValue::Long(42));
        assert_eq!(eval_empty("${42}"), ElValue::Long(42));
        assert_eq!(eval_empty("#{42}"), ElValue::Long(42));
        assert_eq!(eval_empty("3.5"), ElValue::Double(3.5));
        assert_eq!(eval_empty("1e3"), ElValue::Double(1000.0));
        assert_eq!(eval_empty("true"), ElValue::Bool(true));
        assert_eq!(eval_empty("false"), ElValue::Bool(false));
        assert_eq!(eval_empty("null"), ElValue::Null);
        assert_eq!(eval_empty("'hello'"), ElValue::String("hello".into()));
        assert_eq!(
            eval_empty("\"hi there\""),
            ElValue::String("hi there".into())
        );
        assert_eq!(eval_empty(r"'a\nb'"), ElValue::String("a\nb".into()));
    }

    // ---- arithmetic & precedence ---------------------------------------

    #[test]
    fn arithmetic_precedence() {
        assert_eq!(eval_empty("1 + 2 * 3"), ElValue::Long(7));
        assert_eq!(eval_empty("(1 + 2) * 3"), ElValue::Long(9));
        assert_eq!(eval_empty("10 - 4 - 3"), ElValue::Long(3));
        assert_eq!(eval_empty("2 * 3 + 4 * 5"), ElValue::Long(26));
        assert_eq!(eval_empty("-3 + 5"), ElValue::Long(2));
        assert_eq!(eval_empty("7 % 3"), ElValue::Long(1));
        assert_eq!(eval_empty("7 mod 3"), ElValue::Long(1));
    }

    #[test]
    fn division_is_double() {
        // EL '/' (and 'div') always produces a Double.
        assert_eq!(eval_empty("6 / 2"), ElValue::Double(3.0));
        assert_eq!(eval_empty("7 div 2"), ElValue::Double(3.5));
        assert_eq!(eval_empty("1 / 4"), ElValue::Double(0.25));
    }

    #[test]
    fn int_double_coercion() {
        assert_eq!(eval_empty("1 + 2.5"), ElValue::Double(3.5));
        assert_eq!(eval_empty("2 * 1.5"), ElValue::Double(3.0));
        // integral arithmetic stays Long
        assert_eq!(eval_empty("2 + 2"), ElValue::Long(4));
    }

    #[test]
    fn el_plus_is_numeric_not_concat() {
        // In Jakarta EL '+' is arithmetic only: "3" + 4 == 7, not "34".
        assert_eq!(eval_empty("'3' + 4"), ElValue::Long(7));
        assert_eq!(eval_empty("'3' + '4'"), ElValue::Long(7));
        assert_eq!(eval_empty("'1.5' + 1"), ElValue::Double(2.5));
    }

    #[test]
    fn string_concat_via_function() {
        // EL has no concat operator, but a registered function works.
        let mut ctx = ElContext::new();
        ctx.register_function("concat", |args| {
            let mut s = String::new();
            for a in args {
                s.push_str(&a.coerce_to_string());
            }
            Ok(ElValue::String(s))
        });
        assert_eq!(
            eval_str("concat('a', 'b', 3)", &ctx),
            ElValue::String("ab3".into())
        );
    }

    #[test]
    fn divide_by_zero_modulo() {
        assert!(Expression::parse("5 % 0")
            .unwrap()
            .evaluate(&ElContext::new())
            .is_err());
        // float division by zero yields infinity, not an error
        assert_eq!(eval_empty("5.0 / 0"), ElValue::Double(f64::INFINITY));
    }

    // ---- relational & logical ------------------------------------------

    #[test]
    fn relational_operators() {
        assert_eq!(eval_empty("1 < 2"), ElValue::Bool(true));
        assert_eq!(eval_empty("2 <= 2"), ElValue::Bool(true));
        assert_eq!(eval_empty("3 > 5"), ElValue::Bool(false));
        assert_eq!(eval_empty("5 >= 5"), ElValue::Bool(true));
        assert_eq!(eval_empty("1 == 1"), ElValue::Bool(true));
        assert_eq!(eval_empty("1 != 2"), ElValue::Bool(true));
        // keyword forms
        assert_eq!(eval_empty("1 lt 2"), ElValue::Bool(true));
        assert_eq!(eval_empty("2 ge 2"), ElValue::Bool(true));
        assert_eq!(eval_empty("3 eq 3"), ElValue::Bool(true));
        assert_eq!(eval_empty("3 ne 4"), ElValue::Bool(true));
        assert_eq!(eval_empty("4 gt 3"), ElValue::Bool(true));
        assert_eq!(eval_empty("3 le 3"), ElValue::Bool(true));
        // numeric cross-type and string comparison
        assert_eq!(eval_empty("1 == 1.0"), ElValue::Bool(true));
        assert_eq!(eval_empty("'abc' < 'abd'"), ElValue::Bool(true));
        assert_eq!(eval_empty("'10' == 10"), ElValue::Bool(true));
    }

    #[test]
    fn logical_operators() {
        assert_eq!(eval_empty("true && false"), ElValue::Bool(false));
        assert_eq!(eval_empty("true || false"), ElValue::Bool(true));
        assert_eq!(eval_empty("!true"), ElValue::Bool(false));
        assert_eq!(eval_empty("not false"), ElValue::Bool(true));
        assert_eq!(eval_empty("true and (1 < 2)"), ElValue::Bool(true));
        assert_eq!(eval_empty("false or (2 > 1)"), ElValue::Bool(true));
        // precedence: && binds tighter than ||
        assert_eq!(eval_empty("true || false && false"), ElValue::Bool(true));
    }

    #[test]
    fn short_circuit() {
        // The right side would error (modulo by zero) but must not run.
        assert_eq!(eval_empty("false && (1 % 0 == 0)"), ElValue::Bool(false));
        assert_eq!(eval_empty("true || (1 % 0 == 0)"), ElValue::Bool(true));
    }

    // ---- empty operator -------------------------------------------------

    #[test]
    fn empty_operator() {
        assert_eq!(eval_empty("empty null"), ElValue::Bool(true));
        assert_eq!(eval_empty("empty ''"), ElValue::Bool(true));
        assert_eq!(eval_empty("empty 'x'"), ElValue::Bool(false));
        assert_eq!(eval_empty("empty 0"), ElValue::Bool(false));

        let mut ctx = ElContext::new();
        ctx.set_variable("emptyList", ElValue::List(vec![]));
        ctx.set_variable("fullList", ElValue::List(vec![ElValue::Long(1)]));
        ctx.set_variable("emptyMap", ElValue::Map(BTreeMap::new()));
        assert_eq!(eval_str("empty emptyList", &ctx), ElValue::Bool(true));
        assert_eq!(eval_str("empty fullList", &ctx), ElValue::Bool(false));
        assert_eq!(eval_str("empty emptyMap", &ctx), ElValue::Bool(true));
        // empty on a missing identifier (resolves to null) is true
        assert_eq!(eval_str("empty missing", &ctx), ElValue::Bool(true));
    }

    // ---- property / index / nested access ------------------------------

    #[test]
    fn property_access() {
        let mut user = BTreeMap::new();
        user.insert("name".to_string(), ElValue::String("Ada".into()));
        user.insert("age".to_string(), ElValue::Long(36));
        let mut ctx = ElContext::new();
        ctx.set_variable("user", ElValue::Map(user));

        assert_eq!(
            eval_str("${user.name}", &ctx),
            ElValue::String("Ada".into())
        );
        assert_eq!(eval_str("${user.age}", &ctx), ElValue::Long(36));
        // bracket form with string key
        assert_eq!(
            eval_str("${user['name']}", &ctx),
            ElValue::String("Ada".into())
        );
        // missing property -> null
        assert_eq!(eval_str("${user.missing}", &ctx), ElValue::Null);
    }

    #[test]
    fn index_access() {
        let mut ctx = ElContext::new();
        ctx.set_variable(
            "items",
            ElValue::List(vec![
                ElValue::String("a".into()),
                ElValue::String("b".into()),
                ElValue::String("c".into()),
            ]),
        );
        assert_eq!(eval_str("${items[0]}", &ctx), ElValue::String("a".into()));
        assert_eq!(eval_str("${items[2]}", &ctx), ElValue::String("c".into()));
        // out of range -> null
        assert_eq!(eval_str("${items[9]}", &ctx), ElValue::Null);
        // computed index
        assert_eq!(
            eval_str("${items[1 + 1]}", &ctx),
            ElValue::String("c".into())
        );
    }

    #[test]
    fn nested_access() {
        // a.b.c where each level is a map
        let mut c = BTreeMap::new();
        c.insert("c".to_string(), ElValue::Long(99));
        let mut b = BTreeMap::new();
        b.insert("b".to_string(), ElValue::Map(c));
        let mut a = BTreeMap::new();
        a.insert("a".to_string(), ElValue::Map(b));

        let mut ctx = ElContext::new();
        ctx.set_variable("root", ElValue::Map(a));
        assert_eq!(eval_str("${root.a.b.c}", &ctx), ElValue::Long(99));
        // mixed dot and bracket
        assert_eq!(eval_str("${root['a'].b['c']}", &ctx), ElValue::Long(99));
        // navigating through a missing node stays null, no panic
        assert_eq!(eval_str("${root.x.y.z}", &ctx), ElValue::Null);
    }

    // ---- ternary --------------------------------------------------------

    #[test]
    fn ternary() {
        assert_eq!(eval_empty("true ? 1 : 2"), ElValue::Long(1));
        assert_eq!(eval_empty("false ? 1 : 2"), ElValue::Long(2));
        assert_eq!(
            eval_empty("(3 > 2) ? 'yes' : 'no'"),
            ElValue::String("yes".into())
        );
        // nested ternary
        assert_eq!(eval_empty("false ? 1 : (true ? 2 : 3)"), ElValue::Long(2));
        // empty as condition
        assert_eq!(
            eval_empty("empty null ? 'a' : 'b'"),
            ElValue::String("a".into())
        );
    }

    // ---- scopes ---------------------------------------------------------

    #[test]
    fn scope_access() {
        let mut session = BTreeMap::new();
        session.insert("user".to_string(), ElValue::String("bob".into()));
        session.insert("count".to_string(), ElValue::Long(7));

        let mut request = BTreeMap::new();
        request.insert("path".to_string(), ElValue::String("/index".into()));

        let mut ctx = ElContext::new();
        ctx.set_scope("sessionScope", session);
        ctx.set_scope("requestScope", request);

        // explicit scope navigation
        assert_eq!(
            eval_str("${sessionScope.user}", &ctx),
            ElValue::String("bob".into())
        );
        assert_eq!(eval_str("${sessionScope['count']}", &ctx), ElValue::Long(7));
        assert_eq!(
            eval_str("${requestScope.path}", &ctx),
            ElValue::String("/index".into())
        );
        // bare identifier falls back to scope search
        assert_eq!(eval_str("${user}", &ctx), ElValue::String("bob".into()));
        // top-level variable shadows scope entry
        ctx.set_variable("user", ElValue::String("override".into()));
        assert_eq!(
            eval_str("${user}", &ctx),
            ElValue::String("override".into())
        );
    }

    // ---- interpolation --------------------------------------------------

    #[test]
    fn interpolate_mixed_template() {
        let mut ctx = ElContext::new();
        ctx.set_variable("name", ElValue::String("World".into()));
        ctx.set_variable("n", ElValue::Long(3));

        assert_eq!(
            interpolate("Hello, ${name}!", &ctx).unwrap(),
            "Hello, World!"
        );
        assert_eq!(
            interpolate("count=${n + 1} done", &ctx).unwrap(),
            "count=4 done"
        );
        // both ${} and #{}
        assert_eq!(interpolate("a=${n} b=#{n * 2}", &ctx).unwrap(), "a=3 b=6");
        // no fragments -> verbatim
        assert_eq!(interpolate("plain text", &ctx).unwrap(), "plain text");
        // lone '$' not followed by '{' is literal
        assert_eq!(interpolate("price is $5", &ctx).unwrap(), "price is $5");
        // null coerces to empty string
        assert_eq!(interpolate("[${missing}]", &ctx).unwrap(), "[]");
        // brace inside a string literal does not end the fragment
        let mut m = BTreeMap::new();
        m.insert("a}b".to_string(), ElValue::Long(42));
        ctx.set_variable("weird", ElValue::Map(m));
        assert_eq!(interpolate("${weird['a}b']}", &ctx).unwrap(), "42");
    }

    #[test]
    fn interpolate_unterminated_fragment_errors() {
        let ctx = ElContext::new();
        assert!(interpolate("oops ${1 + 2", &ctx).is_err());
    }

    // ---- parse errors ---------------------------------------------------

    #[test]
    fn parse_errors() {
        assert!(Expression::parse("").is_err());
        assert!(Expression::parse("1 +").is_err());
        assert!(Expression::parse("(1 + 2").is_err());
        assert!(Expression::parse("1 2 3").is_err());
        assert!(Expression::parse("* 5").is_err());
        assert!(Expression::parse("a.").is_err());
        assert!(Expression::parse("a[1").is_err());
        assert!(Expression::parse("'unterminated").is_err());
        assert!(Expression::parse("true ? 1").is_err());
        assert!(Expression::parse("@bad").is_err());
    }

    #[test]
    fn eval_errors() {
        // unknown function
        assert!(Expression::parse("nope(1)")
            .unwrap()
            .evaluate(&ElContext::new())
            .is_err());
        // relational against null
        assert!(Expression::parse("null < 1")
            .unwrap()
            .evaluate(&ElContext::new())
            .is_err());
        // non-numeric string in arithmetic
        assert!(Expression::parse("'abc' + 1")
            .unwrap()
            .evaluate(&ElContext::new())
            .is_err());
    }

    // ---- value behaviour ------------------------------------------------

    #[test]
    fn value_display_and_truthiness() {
        assert_eq!(ElValue::Null.to_string(), "");
        assert_eq!(ElValue::Long(5).to_string(), "5");
        assert_eq!(ElValue::Double(2.5).to_string(), "2.5");
        assert_eq!(ElValue::Double(3.0).to_string(), "3.0");
        assert_eq!(ElValue::Bool(true).to_string(), "true");
        assert_eq!(ElValue::String("hi".into()).to_string(), "hi");
        assert_eq!(
            ElValue::List(vec![ElValue::Long(1), ElValue::Long(2)]).to_string(),
            "[1, 2]"
        );

        assert!(!ElValue::Null.is_truthy());
        assert!(!ElValue::Long(0).is_truthy());
        assert!(ElValue::Long(1).is_truthy());
        assert!(!ElValue::String("".into()).is_truthy());
        assert!(!ElValue::String("false".into()).is_truthy());
        assert!(ElValue::String("yes".into()).is_truthy());
    }

    #[test]
    fn error_converts_to_core_error() {
        let e: CoreError = ElError::Eval("boom".into()).into();
        match e {
            CoreError::Other(msg) => assert!(msg.contains("boom")),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn expression_is_reusable() {
        let expr = Expression::parse("${x + 1}").unwrap();
        let mut ctx = ElContext::new();
        ctx.set_variable("x", ElValue::Long(10));
        assert_eq!(expr.evaluate(&ctx).unwrap(), ElValue::Long(11));
        ctx.set_variable("x", ElValue::Long(20));
        assert_eq!(expr.evaluate(&ctx).unwrap(), ElValue::Long(21));
    }
}
