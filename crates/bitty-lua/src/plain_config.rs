//! Generic, host-free plain-data evaluation for per-plugin configuration.

use std::collections::{BTreeMap, BTreeSet};

use phodopus::{
    Context, Table, Value,
    compiler::{
        interning::BasicInterner,
        parse_chunk,
        parser::{
            AssignmentTarget, Block, ConstructorField, Expression, FieldSuffix, ForStatement,
            HeadExpression, PrimaryExpression, RecordKey, SimpleExpression, Statement,
            SuffixedExpression, TableConstructor,
        },
    },
};

use crate::{DriveOutcome, LuaVm, VmError};

/// Owned data accepted at the plugin configuration boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum PlainConfigValue {
    /// Boolean scalar.
    Bool(bool),
    /// Exact Lua integer.
    Integer(i64),
    /// Finite Lua number.
    Number(f64),
    /// UTF-8 string.
    String(String),
    /// String-keyed object.
    Table(BTreeMap<String, PlainConfigValue>),
    /// Dense, one-based Lua array.
    Array(Vec<PlainConfigValue>),
}

/// Bounds applied to source, VM execution, and owned output extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlainConfigLimits {
    /// Maximum source bytes before compilation.
    pub source_bytes: usize,
    /// Maximum VM instructions.
    pub instruction_budget: u64,
    /// Maximum charged wall-clock milliseconds.
    pub wall_budget_ms: u64,
    /// Maximum accounted VM heap bytes.
    pub memory_limit: usize,
    /// Maximum table nesting depth.
    pub max_depth: usize,
    /// Maximum entries in one table.
    pub max_keys: usize,
    /// Maximum scalar/table values in the returned tree.
    pub max_values: usize,
    /// Maximum bytes in one string.
    pub max_string_bytes: usize,
    /// Maximum aggregate output bytes, including bounded structural overhead.
    pub max_aggregate_bytes: usize,
}

impl Default for PlainConfigLimits {
    fn default() -> Self {
        Self {
            source_bytes: 64 * 1024,
            instruction_budget: crate::RC1_INSTRUCTION_BUDGET,
            wall_budget_ms: crate::RC1_WALL_CLOCK_BUDGET_MS,
            memory_limit: crate::RC2_MEMORY_PER_PLUGIN_BYTES,
            max_depth: 16,
            max_keys: 64,
            max_values: 256,
            max_string_bytes: 2048,
            max_aggregate_bytes: 16 * 1024,
        }
    }
}

impl PlainConfigLimits {
    fn validate(self) -> Result<(), VmError> {
        if self.source_bytes == 0
            || self.instruction_budget == 0
            || self.wall_budget_ms == 0
            || self.memory_limit == 0
            || self.max_depth == 0
            || self.max_keys == 0
            || self.max_values == 0
            || self.max_string_bytes == 0
            || self.max_aggregate_bytes == 0
        {
            return Err(VmError::Budget(
                "plain configuration limits must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Failure of source loading, VM execution, or plain-data extraction.
#[derive(Debug, Clone, PartialEq)]
pub enum PlainConfigError {
    /// Source exceeded the configured pre-compile cap.
    SourceTooLarge,
    /// VM construction or budget failure.
    Vm(VmError),
    /// Lua compile/runtime failure.
    Lua(String),
    /// Returned value has an invalid shape or type.
    Shape(String),
    /// Returned data exceeded an extraction bound.
    Limit(String),
}

impl std::fmt::Display for PlainConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceTooLarge => write!(f, "configuration source exceeds the byte limit"),
            Self::Vm(error) => write!(f, "{error}"),
            Self::Lua(message) | Self::Shape(message) | Self::Limit(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for PlainConfigError {}

/// Fresh restricted VM used only for generic plugin configuration.
pub struct ConfigLuaVm {
    vm: LuaVm,
    limits: PlainConfigLimits,
}

impl ConfigLuaVm {
    /// Construct a fresh configuration VM. It has no host bridge state, module
    /// root, plugin services, or retained side-effecting standard library.
    pub fn with_limits(id: impl Into<String>, limits: PlainConfigLimits) -> Result<Self, VmError> {
        limits.validate()?;
        Ok(Self {
            vm: LuaVm::with_config_budgets(
                id,
                limits.instruction_budget,
                limits.wall_budget_ms,
                0,
                limits.memory_limit,
            ),
            limits,
        })
    }

    /// Evaluate source and return an owned string-keyed table or dense array.
    pub fn eval_plain_config(
        &mut self,
        source: &str,
    ) -> Result<PlainConfigValue, PlainConfigError> {
        if source.len() > self.limits.source_bytes {
            return Err(PlainConfigError::SourceTooLarge);
        }
        reject_duplicate_literal_keys(source)?;
        match self.vm.drive_chunk(source).map_err(PlainConfigError::Vm)? {
            DriveOutcome::Suspended { reason, .. } => {
                Err(PlainConfigError::Vm(VmError::Suspended { reason }))
            }
            DriveOutcome::Failed { message } => Err(PlainConfigError::Lua(message)),
            DriveOutcome::Ready { stashed } => self.vm.take_plain_result(stashed, self.limits),
        }
    }
}

fn reject_duplicate_literal_keys(source: &str) -> Result<(), PlainConfigError> {
    let chunk = parse_chunk(source.as_bytes(), BasicInterner::default())
        .map_err(|error| PlainConfigError::Lua(format!("{error}")))?;
    check_block(&chunk.block)
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LiteralKey {
    Integer(i64),
    String(Vec<u8>),
}

fn check_block<S>(block: &Block<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    for statement in &block.statements {
        check_statement(&statement.inner)?;
    }
    if let Some(return_statement) = &block.return_statement {
        for expression in &return_statement.returns {
            check_expression(expression)?;
        }
    }
    Ok(())
}

fn check_statement<S>(statement: &Statement<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    match statement {
        Statement::If(if_statement) => {
            check_expression(&if_statement.if_part.0)?;
            check_block(&if_statement.if_part.1)?;
            for (condition, block) in &if_statement.else_if_parts {
                check_expression(condition)?;
                check_block(block)?;
            }
            if let Some(block) = &if_statement.else_part {
                check_block(block)?;
            }
        }
        Statement::While(while_statement) => {
            check_expression(&while_statement.condition)?;
            check_block(&while_statement.block)?;
        }
        Statement::Do(block) => check_block(block)?,
        Statement::For(for_statement) => match for_statement {
            ForStatement::Numeric {
                initial,
                limit,
                step,
                body,
                ..
            } => {
                check_expression(initial)?;
                check_expression(limit)?;
                if let Some(step) = step {
                    check_expression(step)?;
                }
                check_block(body)?;
            }
            ForStatement::Generic {
                arguments, body, ..
            } => {
                for argument in arguments {
                    check_expression(argument)?;
                }
                check_block(body)?;
            }
        },
        Statement::Repeat(repeat_statement) => {
            check_block(&repeat_statement.body)?;
            check_expression(&repeat_statement.until)?;
        }
        Statement::Function(function_statement) => check_function(&function_statement.definition)?,
        Statement::LocalFunction(function_statement) => {
            check_function(&function_statement.definition)?
        }
        Statement::LocalStatement(local_statement) => {
            for expression in &local_statement.values {
                check_expression(expression)?;
            }
        }
        Statement::FunctionCall(function_call) => {
            check_suffixed_expression(&function_call.head)?;
            check_call(&function_call.call)?;
        }
        Statement::Assignment(assignment) => {
            for target in &assignment.targets {
                match target {
                    AssignmentTarget::Name(_) => {}
                    AssignmentTarget::Field(expression, field) => {
                        check_suffixed_expression(expression)?;
                        check_field(field)?;
                    }
                }
            }
            for expression in &assignment.values {
                check_expression(expression)?;
            }
        }
        Statement::Label(_) | Statement::Break | Statement::Goto(_) => {}
    }
    Ok(())
}

fn check_function<S>(
    function: &phodopus::compiler::parser::FunctionDefinition<S>,
) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    check_block(&function.body)
}

fn check_expression<S>(expression: &Expression<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    check_head(&expression.head)?;
    for (_, expression) in &expression.tail {
        check_expression(expression)?;
    }
    Ok(())
}

fn check_head<S>(head: &HeadExpression<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    match head {
        HeadExpression::Simple(simple) => match simple {
            SimpleExpression::TableConstructor(table) => check_table(table)?,
            SimpleExpression::Function(function) => check_function(function)?,
            SimpleExpression::Suffixed(expression) => check_suffixed_expression(expression)?,
            SimpleExpression::Float(_)
            | SimpleExpression::Integer(_)
            | SimpleExpression::String(_)
            | SimpleExpression::Nil
            | SimpleExpression::True
            | SimpleExpression::False
            | SimpleExpression::VarArgs => {}
        },
        HeadExpression::UnaryOperator(_, expression) => check_expression(expression)?,
    }
    Ok(())
}

fn check_suffixed_expression<S>(expression: &SuffixedExpression<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    match &expression.primary {
        PrimaryExpression::Name(_) => {}
        PrimaryExpression::GroupedExpression(expression) => check_expression(expression)?,
    }
    for suffix in &expression.suffixes {
        match suffix {
            phodopus::compiler::parser::SuffixPart::Field(field) => check_field(field)?,
            phodopus::compiler::parser::SuffixPart::Call(call) => check_call(call)?,
        }
    }
    Ok(())
}

fn check_field<S>(field: &FieldSuffix<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    if let FieldSuffix::Indexed(expression) = field {
        check_expression(expression)?;
    }
    Ok(())
}

fn check_call<S>(call: &phodopus::compiler::parser::CallSuffix<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    let arguments = match call {
        phodopus::compiler::parser::CallSuffix::Method(_, arguments)
        | phodopus::compiler::parser::CallSuffix::Function(arguments) => arguments,
    };
    for argument in arguments {
        check_expression(argument)?;
    }
    Ok(())
}

fn check_table<S>(table: &TableConstructor<S>) -> Result<(), PlainConfigError>
where
    S: AsRef<[u8]> + Clone,
{
    let mut keys = BTreeSet::new();
    let mut next_array_index = 1_i64;
    for field in &table.fields {
        match field {
            ConstructorField::Array(expression) => {
                if !keys.insert(LiteralKey::Integer(next_array_index)) {
                    return Err(PlainConfigError::Shape(
                        "duplicate configuration key".to_string(),
                    ));
                }
                next_array_index = next_array_index.checked_add(1).ok_or_else(|| {
                    PlainConfigError::Limit("configuration array index overflow".to_string())
                })?;
                check_expression(expression)?;
            }
            ConstructorField::Record(key, value) => {
                let key = literal_key(key).ok_or_else(|| {
                    PlainConfigError::Shape(
                        "configuration keys must be literal strings or integer-compatible numbers"
                            .to_string(),
                    )
                })?;
                if !keys.insert(key) {
                    return Err(PlainConfigError::Shape(
                        "duplicate configuration key".to_string(),
                    ));
                }
                check_expression(value)?;
            }
        }
    }
    Ok(())
}

fn literal_key<S>(key: &RecordKey<S>) -> Option<LiteralKey>
where
    S: AsRef<[u8]> + Clone,
{
    match key {
        RecordKey::Named(name) => Some(LiteralKey::String(name.as_ref().to_vec())),
        RecordKey::Indexed(expression) => literal_expression(expression),
    }
}

fn literal_expression<S>(expression: &Expression<S>) -> Option<LiteralKey>
where
    S: AsRef<[u8]> + Clone,
{
    if !expression.tail.is_empty() {
        return None;
    }
    match expression.head.as_ref() {
        HeadExpression::Simple(SimpleExpression::Integer(value)) => {
            Some(LiteralKey::Integer(*value))
        }
        HeadExpression::Simple(SimpleExpression::Float(value)) => finite_integral_float_key(*value),
        HeadExpression::Simple(SimpleExpression::String(value)) => {
            Some(LiteralKey::String(value.as_ref().to_vec()))
        }
        HeadExpression::Simple(SimpleExpression::Suffixed(expression))
            if expression.suffixes.is_empty() =>
        {
            match &expression.primary {
                PrimaryExpression::GroupedExpression(expression) => literal_expression(expression),
                PrimaryExpression::Name(_) => None,
            }
        }
        HeadExpression::UnaryOperator(operator, expression) => match operator {
            phodopus::compiler::parser::UnaryOperator::Minus => {
                match literal_expression(expression) {
                    Some(LiteralKey::Integer(value)) => {
                        Some(LiteralKey::Integer(value.wrapping_neg()))
                    }
                    _ => None,
                }
            }
            phodopus::compiler::parser::UnaryOperator::BitNot => {
                match literal_expression(expression) {
                    Some(LiteralKey::Integer(value)) => Some(LiteralKey::Integer(!value)),
                    _ => None,
                }
            }
            phodopus::compiler::parser::UnaryOperator::Not
            | phodopus::compiler::parser::UnaryOperator::Len => None,
        },
        _ => None,
    }
}

/// Lua uses the same table key identity for an integer and an exactly integral
/// number. Keep the AST duplicate check bounded to identities representable by
/// the VM's `i64` integer values. Unsupported indexed expressions are rejected
/// before runtime evaluation because Lua may normalize them to a supported key.
fn finite_integral_float_key(value: f64) -> Option<LiteralKey> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }

    // `as i64` saturates outside the integer range, so check the range before
    // the cast rather than allowing 2^63 to alias i64::MAX.
    let min = i64::MIN as f64;
    let max_exclusive = -(i64::MIN as f64);
    if !(min..max_exclusive).contains(&value) {
        return None;
    }
    let integer = value as i64;
    (integer as f64 == value).then_some(LiteralKey::Integer(integer))
}

pub(crate) fn capture_plain_value<'gc>(
    ctx: Context<'gc>,
    value: Value<'gc>,
    limits: PlainConfigLimits,
) -> Result<PlainConfigValue, PlainConfigError> {
    let mut state = CaptureState {
        limits,
        values: 0,
        aggregate_bytes: 0,
        active_tables: Vec::new(),
    };
    state.capture(ctx, value, 0)
}

struct CaptureState<'gc> {
    limits: PlainConfigLimits,
    values: usize,
    aggregate_bytes: usize,
    active_tables: Vec<Table<'gc>>,
}

impl<'gc> CaptureState<'gc> {
    fn charge(&mut self, bytes: usize) -> Result<(), PlainConfigError> {
        self.aggregate_bytes = self
            .aggregate_bytes
            .checked_add(bytes)
            .ok_or_else(|| PlainConfigError::Limit("aggregate output overflow".to_string()))?;
        if self.aggregate_bytes > self.limits.max_aggregate_bytes {
            return Err(PlainConfigError::Limit(
                "aggregate output exceeds limit".to_string(),
            ));
        }
        Ok(())
    }

    fn capture(
        &mut self,
        ctx: Context<'gc>,
        value: Value<'gc>,
        depth: usize,
    ) -> Result<PlainConfigValue, PlainConfigError> {
        if depth > self.limits.max_depth {
            return Err(PlainConfigError::Limit(
                "configuration nesting exceeds depth limit".to_string(),
            ));
        }
        if self.values >= self.limits.max_values {
            return Err(PlainConfigError::Limit(
                "configuration value count exceeds limit".to_string(),
            ));
        }
        self.values = self.values.checked_add(1).ok_or_else(|| {
            PlainConfigError::Limit("configuration value count overflow".to_string())
        })?;
        match value {
            Value::Boolean(value) => {
                self.charge(1)?;
                Ok(PlainConfigValue::Bool(value))
            }
            Value::Integer(value) => {
                self.charge(std::mem::size_of::<i64>())?;
                Ok(PlainConfigValue::Integer(value))
            }
            Value::Number(value) if value.is_finite() => {
                self.charge(std::mem::size_of::<f64>())?;
                Ok(PlainConfigValue::Number(value))
            }
            Value::Number(_) => Err(PlainConfigError::Shape(
                "non-finite numbers are not configuration data".to_string(),
            )),
            Value::String(value) => {
                let bytes = value.as_bytes();
                if bytes.len() > self.limits.max_string_bytes {
                    return Err(PlainConfigError::Limit(
                        "configuration string exceeds limit".to_string(),
                    ));
                }
                self.charge(bytes.len())?;
                let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
                    PlainConfigError::Shape("configuration strings must be UTF-8".to_string())
                })?;
                Ok(PlainConfigValue::String(text))
            }
            Value::Table(table) => self.capture_table(ctx, table, depth),
            other => Err(PlainConfigError::Shape(format!(
                "unsupported configuration value type '{}'",
                other.type_name()
            ))),
        }
    }

    fn capture_table(
        &mut self,
        ctx: Context<'gc>,
        table: Table<'gc>,
        depth: usize,
    ) -> Result<PlainConfigValue, PlainConfigError> {
        if self.active_tables.contains(&table) {
            return Err(PlainConfigError::Shape(
                "cyclic configuration table".to_string(),
            ));
        }
        if table.metatable().is_some() {
            return Err(PlainConfigError::Shape(
                "configuration tables may not have metatables".to_string(),
            ));
        }
        self.charge(1)?;
        self.active_tables.push(table);
        let result = self.capture_table_pairs(ctx, table, depth);
        self.active_tables.pop();
        result
    }

    fn capture_table_pairs(
        &mut self,
        ctx: Context<'gc>,
        table: Table<'gc>,
        depth: usize,
    ) -> Result<PlainConfigValue, PlainConfigError> {
        let mut kind = None;
        let mut table_keys = 0;
        let mut arrays = BTreeMap::new();
        let mut maps = BTreeMap::new();
        for (key, value) in table.iter() {
            if table_keys >= self.limits.max_keys {
                return Err(PlainConfigError::Limit(
                    "configuration table key count exceeds limit".to_string(),
                ));
            }
            table_keys = table_keys.checked_add(1).ok_or_else(|| {
                PlainConfigError::Limit("configuration key count overflow".to_string())
            })?;
            self.charge(2)?;

            let is_integer = matches!(key, Value::Integer(_));
            let is_string = matches!(key, Value::String(_));
            if !is_integer && !is_string {
                return Err(PlainConfigError::Shape(
                    "configuration tables may not mix array and map keys".to_string(),
                ));
            }
            if kind.is_some_and(|was_integer| was_integer != is_integer) {
                return Err(PlainConfigError::Shape(
                    "configuration tables may not mix array and map keys".to_string(),
                ));
            }
            kind = Some(is_integer);

            if is_integer {
                let Value::Integer(index) = key else {
                    unreachable!()
                };
                let Some(slot) = index.checked_sub(1).and_then(|i| usize::try_from(i).ok()) else {
                    return Err(PlainConfigError::Shape(
                        "configuration arrays must use contiguous positive integer keys"
                            .to_string(),
                    ));
                };
                if slot >= self.limits.max_keys {
                    return Err(PlainConfigError::Shape(
                        "configuration arrays must use contiguous positive integer keys"
                            .to_string(),
                    ));
                }
                if arrays.contains_key(&slot) {
                    return Err(PlainConfigError::Shape(
                        "duplicate configuration array key".to_string(),
                    ));
                }
                arrays.insert(slot, self.capture(ctx, value, depth + 1)?);
            } else {
                let Value::String(key) = key else {
                    unreachable!()
                };
                let bytes = key.as_bytes();
                if bytes.len() > self.limits.max_string_bytes {
                    return Err(PlainConfigError::Limit(
                        "configuration key exceeds string limit".to_string(),
                    ));
                }
                self.charge(bytes.len())?;
                let key = String::from_utf8(bytes.to_vec()).map_err(|_| {
                    PlainConfigError::Shape("configuration keys must be UTF-8".to_string())
                })?;
                if maps.contains_key(&key) {
                    return Err(PlainConfigError::Shape(
                        "duplicate configuration map key".to_string(),
                    ));
                }
                maps.insert(key, self.capture(ctx, value, depth + 1)?);
            }
        }
        if kind == Some(true) {
            let expected_len = arrays
                .keys()
                .next_back()
                .and_then(|last| last.checked_add(1))
                .unwrap_or(0);
            if arrays.len() != expected_len {
                return Err(PlainConfigError::Shape(
                    "configuration arrays must be dense".to_string(),
                ));
            }
            Ok(PlainConfigValue::Array(arrays.into_values().collect()))
        } else {
            Ok(PlainConfigValue::Table(maps))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(source: &str) -> Result<PlainConfigValue, PlainConfigError> {
        let mut vm = ConfigLuaVm::with_limits("test.config", PlainConfigLimits::default())
            .expect("valid limits");
        vm.eval_plain_config(source)
    }

    #[test]
    fn extracts_owned_plain_data() {
        assert_eq!(
            eval("return { enabled = true, count = 4, tags = {'a', 'b'} }").expect("plain data"),
            PlainConfigValue::Table(BTreeMap::from([
                ("count".to_string(), PlainConfigValue::Integer(4)),
                ("enabled".to_string(), PlainConfigValue::Bool(true)),
                (
                    "tags".to_string(),
                    PlainConfigValue::Array(vec![
                        PlainConfigValue::String("a".to_string()),
                        PlainConfigValue::String("b".to_string()),
                    ]),
                ),
            ]))
        );
    }

    #[test]
    fn forbidden_globals_are_absent() {
        for source in [
            "return { value = io.open }",
            "return { value = os.time }",
            "return { value = debug.getinfo }",
            "return { value = package.loaded }",
            "return { value = math.random() }",
            "return { value = require('x') }",
            "return { value = load('return 1') }",
            "return { value = dofile('x') }",
        ] {
            assert!(
                eval(source).is_err(),
                "source unexpectedly succeeded: {source}"
            );
        }
    }

    #[test]
    fn rejects_shapes_cycles_and_non_finite_numbers() {
        for source in [
            "return 1",
            "return { [1] = 'x', name = 'mixed' }",
            "return { value = 0/0 }",
            "local x = {}; x.self = x; return x",
            "return setmetatable({}, {})",
            "return { value = 1, value = 2 }",
            r#"return { ["show"] = true, ["show"] = false }"#,
            r#"return { ["show"] = true, ["sh\111w"] = false }"#,
        ] {
            assert!(
                eval(source).is_err(),
                "source unexpectedly succeeded: {source}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_integral_float_key_aliases() {
        for source in [
            "return { [1] = true, [1.0] = false }",
            "return { [1] = true, [1e0] = false }",
            "return { [1] = true, [0x1p0] = false }",
            "return { [0] = true, [-0.0] = false }",
            "return { [-1] = true, [-1.0] = false }",
            "return { [1000] = true, [1e3] = false }",
            "return { [1] = true, [1.0 + 0.0] = false }",
            "return { [1] = true, [+1.0] = false }",
        ] {
            assert!(
                eval(source).is_err(),
                "numeric key alias unexpectedly succeeded: {source}"
            );
        }

        assert_eq!(
            eval("return { value = 1.5 }").expect("nonintegral number value is valid"),
            PlainConfigValue::Table(BTreeMap::from([(
                "value".to_string(),
                PlainConfigValue::Number(1.5),
            )]))
        );
        assert!(finite_integral_float_key(1.5).is_none());
    }

    #[test]
    fn rejects_computed_indexed_keys_during_ast_validation() {
        for source in [
            "return { [1.0 + 0.0] = true }",
            "return { [1 + 0] = true }",
            "return { [1 << 0] = true }",
            "local key = 1; return { [key] = true }",
        ] {
            assert!(
                reject_duplicate_literal_keys(source).is_err(),
                "computed indexed key unexpectedly passed AST validation: {source}"
            );
        }
    }

    #[test]
    fn preserves_string_and_unary_numeric_key_forms() {
        for source in [
            r#"return { ["show"] = true }"#,
            "return { [1] = true }",
            "return { [1.0] = true }",
            "return { [-1] = true }",
            "return { [-1.0] = true }",
            "return { [~1] = true }",
            "return { [~1.0] = true }",
            "return { [-~1] = true }",
        ] {
            assert!(
                reject_duplicate_literal_keys(source).is_ok(),
                "supported key form unexpectedly failed AST validation: {source}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_numeric_index_expressions() {
        for source in ["return { [1.0 + 0.0] = true }", "return { [+1.0] = true }"] {
            assert!(
                eval(source).is_err(),
                "unsupported numeric index unexpectedly succeeded: {source}"
            );
        }
    }

    #[test]
    fn syntax_aware_duplicate_check_ignores_nested_code_and_strings() {
        assert_eq!(
            eval(
                r#"
                    local make = function()
                        local nested = { value = true }
                        return { show = nested.value }
                    end
                    return make()
                "#,
            )
            .expect("nested function is valid configuration code"),
            PlainConfigValue::Table(BTreeMap::from([(
                "show".to_string(),
                PlainConfigValue::Bool(true),
            )]))
        );
        assert_eq!(
            eval(
                r#"
                    return {
                        show = [=[text containing braces: { } and quotes: " ']=]
                    }
                "#,
            )
            .expect("long-bracket strings are valid configuration data"),
            PlainConfigValue::Table(BTreeMap::from([(
                "show".to_string(),
                PlainConfigValue::String(
                    "text containing braces: { } and quotes: \" '".to_string(),
                ),
            )]))
        );
        assert_eq!(
            eval(r#"return { show = "line\nwith \"quotes\"" }"#)
                .expect("escaped strings are valid configuration data"),
            PlainConfigValue::Table(BTreeMap::from([(
                "show".to_string(),
                PlainConfigValue::String("line\nwith \"quotes\"".to_string()),
            )]))
        );
        assert!(eval("return { show = [=[unterminated } ").is_err());
    }

    #[test]
    fn applies_source_and_value_bounds() {
        let limits = PlainConfigLimits {
            source_bytes: 4,
            ..PlainConfigLimits::default()
        };
        let mut vm = ConfigLuaVm::with_limits("test.config", limits).expect("valid limits");
        assert!(matches!(
            vm.eval_plain_config("return {}"),
            Err(PlainConfigError::SourceTooLarge)
        ));
        let limits = PlainConfigLimits {
            max_values: 2,
            ..PlainConfigLimits::default()
        };
        let mut vm = ConfigLuaVm::with_limits("test.config", limits).expect("valid limits");
        assert!(matches!(
            vm.eval_plain_config("return {a = {b = true}}"),
            Err(PlainConfigError::Limit(_))
        ));
    }

    #[test]
    fn rejects_excessive_table_keys_incrementally() {
        let limits = PlainConfigLimits {
            max_keys: 2,
            ..PlainConfigLimits::default()
        };
        let mut vm = ConfigLuaVm::with_limits("test.config", limits).expect("valid limits");
        assert!(matches!(
            vm.eval_plain_config(
                "local t = {}; for i = 1, 1000 do t[i] = string.rep('x', 1024) end; return t"
            ),
            Err(PlainConfigError::Limit(_))
        ));
    }

    #[test]
    fn applies_key_bound_per_table() {
        let limits = PlainConfigLimits {
            max_keys: 1,
            ..PlainConfigLimits::default()
        };
        let mut vm = ConfigLuaVm::with_limits("test.config", limits).expect("valid limits");
        assert!(
            vm.eval_plain_config("return { outer = { inner = true } }")
                .is_ok()
        );
    }
}
