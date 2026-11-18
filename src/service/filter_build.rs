//! Compile a filter AST into a parameterised `sea_orm` `Condition` for the
//! `/preview` endpoint.
//!
//! Every value is bound (never interpolated), so this is SQL-injection-safe by
//! construction — the same guarantee the AST gives the stored form. v1 supports
//! `Events`-table fields only; joined attributes (`monitor_name`) and `regexp`
//! operators are rejected with a 400 for preview (they are still accepted for
//! stored filters that `zmfilter.pl` executes).

use sea_orm::sea_query::SimpleExpr;
use sea_orm::{ColumnTrait, Condition};

use crate::dto::request::filter_ast::{FilterExpr, FilterField, FilterOp, MatchOp};
use crate::entity::events;
use crate::error::{AppError, AppResult};

use super::filter_field::{meta, FieldColumn, ValueKind};

fn bad(msg: impl Into<String>) -> AppError {
    AppError::BadRequestError(msg.into())
}

/// Build a `sea_orm` `Condition` from the predicate tree.
pub fn build_condition(expr: &FilterExpr) -> AppResult<Condition> {
    match expr {
        FilterExpr::Group { match_op, rules } => {
            let mut cond = match match_op {
                MatchOp::All => Condition::all(),
                MatchOp::Any => Condition::any(),
            };
            for rule in rules {
                cond = cond.add(build_condition(rule)?);
            }
            Ok(cond)
        }
        FilterExpr::Condition { field, op, value } => {
            Ok(Condition::all().add(build_leaf(*field, *op, value)?))
        }
    }
}

/// Resolve a sort field to an `Events` column (preview is Events-only).
pub fn event_sort_column(field: FilterField) -> AppResult<events::Column> {
    match meta(field).column {
        FieldColumn::Event(c) => Ok(c),
        FieldColumn::Monitor(_) => Err(bad("sorting on monitor_name is not supported in preview")),
    }
}

fn build_leaf(
    field: FilterField,
    op: FilterOp,
    value: &Option<serde_json::Value>,
) -> AppResult<SimpleExpr> {
    let m = meta(field);
    let col = match m.column {
        FieldColumn::Event(c) => c,
        FieldColumn::Monitor(_) => {
            return Err(bad(
                "filtering on monitor_name is not supported in preview yet",
            ));
        }
    };
    let expr = match op {
        FilterOp::Eq => col.eq(scalar(value, m.kind)?),
        FilterOp::Ne => col.ne(scalar(value, m.kind)?),
        FilterOp::Gt => col.gt(scalar(value, m.kind)?),
        FilterOp::Gte => col.gte(scalar(value, m.kind)?),
        FilterOp::Lt => col.lt(scalar(value, m.kind)?),
        FilterOp::Lte => col.lte(scalar(value, m.kind)?),
        FilterOp::Like => col.like(string(value)?),
        FilterOp::NotLike => col.not_like(string(value)?),
        FilterOp::In => col.is_in(list(value, m.kind)?),
        FilterOp::NotIn => col.is_not_in(list(value, m.kind)?),
        FilterOp::IsNull => col.is_null(),
        FilterOp::IsNotNull => col.is_not_null(),
        FilterOp::Regexp | FilterOp::NotRegexp => {
            return Err(bad("regexp operators are not supported in preview"));
        }
    };
    Ok(expr)
}

fn require(value: &Option<serde_json::Value>) -> AppResult<&serde_json::Value> {
    value
        .as_ref()
        .ok_or_else(|| bad("operator requires a value"))
}

fn string(value: &Option<serde_json::Value>) -> AppResult<String> {
    require(value)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| bad("operator requires a string value"))
}

fn list(value: &Option<serde_json::Value>, kind: ValueKind) -> AppResult<Vec<sea_orm::Value>> {
    let arr = require(value)?
        .as_array()
        .ok_or_else(|| bad("in/not_in require an array value"))?;
    arr.iter().map(|v| scalar_one(v, kind)).collect()
}

fn scalar(value: &Option<serde_json::Value>, kind: ValueKind) -> AppResult<sea_orm::Value> {
    scalar_one(require(value)?, kind)
}

/// Coerce a JSON scalar into a typed, bound `sea_orm::Value`.
fn scalar_one(v: &serde_json::Value, kind: ValueKind) -> AppResult<sea_orm::Value> {
    Ok(match kind {
        ValueKind::Int => v
            .as_i64()
            .map(sea_orm::Value::from)
            .ok_or_else(|| bad("expected an integer value"))?,
        ValueKind::Decimal => v
            .as_f64()
            .map(sea_orm::Value::from)
            .ok_or_else(|| bad("expected a numeric value"))?,
        ValueKind::Str => v
            .as_str()
            .map(|s| sea_orm::Value::from(s.to_string()))
            .ok_or_else(|| bad("expected a string value"))?,
        ValueKind::DateTime => {
            let s = v
                .as_str()
                .ok_or_else(|| bad("expected an ISO-8601 datetime string"))?;
            let dt = s
                .parse::<jiff::Timestamp>()
                .map_err(|_| bad("invalid datetime; expected ISO-8601 / RFC-3339"))?;
            // Event date columns are server-local wall-clock time (#173).
            sea_orm::Value::from(crate::util::utc_to_datetime(dt))
        }
        ValueKind::Bool => {
            let b = match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::Number(n) => n.as_u64() == Some(1),
                _ => return Err(bad("expected a boolean value")),
            };
            sea_orm::Value::from(if b { 1i32 } else { 0i32 })
        }
    })
}
