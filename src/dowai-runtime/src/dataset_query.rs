//! Planning of a lazy dataset read (ENT-041).
//!
//! The worker sends `select` (projection) and `filters` (predicates) as data. This module
//! validates them against the relation's columns and decides, per predicate, whether SQL
//! executes it (pushdown) or the worker applies it to the fetched rows (fallback, reported
//! as a warning). Nothing from the request is spliced into SQL text: operators come from a
//! closed set and values are bound as typed parameters by `dowai_postgres`.

use dowai_core::DowaiError;
use dowai_postgres::{FilterOp, FilterValue, SqlFilter};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

pub(crate) const DATASET_INVALID_QUERY: &str = "DOWAI_DATASET_INVALID_QUERY";
pub(crate) const DATASET_FILTER_UNSUPPORTED: &str = "DOWAI_DATASET_FILTER_UNSUPPORTED";
/// Prefix of the warning recorded when a predicate is applied by the worker, not by SQL.
pub(crate) const FILTER_NOT_PUSHED: &str = "DOWAI_DATASET_FILTER_NOT_PUSHED";

/// One predicate as the worker sends it: `{"column": ..., "op": ..., "value": ...}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FilterRequest {
    pub column: String,
    pub op: String,
    #[serde(default)]
    pub value: Value,
}

/// What one batch read does: which columns are fetched, which predicates SQL runs and which
/// predicates the worker must apply to the fetched rows.
#[derive(Debug, Default)]
pub(crate) struct QueryPlan {
    /// Columns fetched, in relation order: the output plus columns needed by residual filters.
    pub fetch: Vec<(String, String)>,
    /// Predicates executed by SQL in the `WHERE` clause.
    pub pushed: Vec<SqlFilter>,
    /// Indexes, in the request, of the predicates the worker applies after fetching.
    pub residual: Vec<usize>,
    /// Warnings for predicates that were not pushed down.
    pub warnings: Vec<String>,
}

fn invalid(message: &str) -> DowaiError {
    DowaiError::DatasetAccess {
        code: DATASET_INVALID_QUERY,
        message: message.to_owned(),
    }
}

fn parse_op(text: &str) -> Result<FilterOp, DowaiError> {
    match text {
        "=" => Ok(FilterOp::Eq),
        "!=" => Ok(FilterOp::NotEq),
        "<" => Ok(FilterOp::Lt),
        "<=" => Ok(FilterOp::LtEq),
        ">" => Ok(FilterOp::Gt),
        ">=" => Ok(FilterOp::GtEq),
        "is_null" => Ok(FilterOp::IsNull),
        "is_not_null" => Ok(FilterOp::IsNotNull),
        _ => Err(invalid("filter operator is not supported")),
    }
}

fn is_equality(op: FilterOp) -> bool {
    matches!(op, FilterOp::Eq | FilterOp::NotEq)
}

/// Typed value for a column of a native type, or `None` when the type is read as text and
/// therefore cannot be compared by SQL with the same semantics as the value.
fn native_value(
    data_type: &str,
    op: FilterOp,
    value: &Value,
) -> Result<Option<FilterValue>, DowaiError> {
    let typed = match data_type {
        "bool" => {
            if !is_equality(op) {
                return Err(invalid("boolean columns accept only = and !="));
            }
            FilterValue::Bool(
                value
                    .as_bool()
                    .ok_or_else(|| invalid("filter value does not match the column type"))?,
            )
        }
        "int2" | "int4" | "int8" => FilterValue::Int(
            value
                .as_i64()
                .ok_or_else(|| invalid("filter value does not match the column type"))?,
        ),
        "float4" | "float8" => FilterValue::Float(
            value
                .as_f64()
                .ok_or_else(|| invalid("filter value does not match the column type"))?,
        ),
        "text" | "varchar" | "bpchar" | "name" => FilterValue::Text(
            value
                .as_str()
                .ok_or_else(|| invalid("filter value does not match the column type"))?
                .to_owned(),
        ),
        _ => return Ok(None),
    };
    Ok(Some(typed))
}

/// Validates the projection and predicates against `columns` (name, type) and plans the read.
pub(crate) fn plan(
    columns: &[(String, String)],
    select: Option<&[String]>,
    filters: &[FilterRequest],
) -> Result<QueryPlan, DowaiError> {
    let type_of = |name: &str| {
        columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, data_type)| data_type.as_str())
    };
    let output: Vec<String> = match select {
        None => columns.iter().map(|(name, _)| name.clone()).collect(),
        Some(names) => {
            if names.is_empty() {
                return Err(invalid("select must name at least one column"));
            }
            let mut seen = BTreeSet::new();
            for name in names {
                if type_of(name).is_none() {
                    return Err(invalid("select names a column that does not exist"));
                }
                if !seen.insert(name.as_str()) {
                    return Err(invalid("select names the same column twice"));
                }
            }
            names.to_vec()
        }
    };

    let mut needed: BTreeSet<String> = output.iter().cloned().collect();
    let mut plan = QueryPlan::default();
    for (index, request) in filters.iter().enumerate() {
        let data_type = type_of(&request.column)
            .ok_or_else(|| invalid("filter names a column that does not exist"))?;
        let op = parse_op(&request.op)?;
        if matches!(op, FilterOp::IsNull | FilterOp::IsNotNull) {
            if !request.value.is_null() {
                return Err(invalid("is_null and is_not_null take no value"));
            }
            plan.pushed.push(SqlFilter {
                column: request.column.clone(),
                op,
                value: FilterValue::None,
            });
            continue;
        }
        if request.value.is_null() {
            return Err(invalid("comparison filters need a non-null value"));
        }
        if let Some(value) = native_value(data_type, op, &request.value)? {
            plan.pushed.push(SqlFilter {
                column: request.column.clone(),
                op,
                value,
            });
            continue;
        }
        // Text-read column (numeric, dates, UUID, ...): the worker compares the text form, which
        // is exact only for equality. Ordered comparisons would order text, not values.
        if !is_equality(op) {
            return Err(DowaiError::DatasetAccess {
                code: DATASET_FILTER_UNSUPPORTED,
                message: "ordered filters are not supported on this column type".to_owned(),
            });
        }
        if !request.value.is_string() {
            return Err(invalid("filter value must be text for this column type"));
        }
        plan.residual.push(index);
        needed.insert(request.column.clone());
        plan.warnings.push(format!(
            "{FILTER_NOT_PUSHED}: filter on column \"{}\" of type {data_type} is applied by the worker to the text form, not by SQL",
            request.column
        ));
    }

    plan.fetch = columns
        .iter()
        .filter(|(name, _)| needed.contains(name))
        .cloned()
        .collect();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns() -> Vec<(String, String)> {
        vec![
            ("id".to_owned(), "int4".to_owned()),
            ("label".to_owned(), "text".to_owned()),
            ("amount".to_owned(), "numeric".to_owned()),
        ]
    }

    fn request(column: &str, op: &str, value: Value) -> FilterRequest {
        FilterRequest {
            column: column.to_owned(),
            op: op.to_owned(),
            value,
        }
    }

    #[test]
    fn native_comparisons_are_pushed_and_nothing_is_residual() {
        let plan = plan(
            &columns(),
            Some(&["id".to_owned()]),
            &[
                request("id", ">", json!(3)),
                request("label", "=", json!("a")),
            ],
        )
        .expect("plan");
        assert_eq!(plan.pushed.len(), 2);
        assert!(plan.residual.is_empty());
        assert!(plan.warnings.is_empty());
        // label is needed by no output and by no residual: it is not fetched.
        assert_eq!(plan.fetch, vec![("id".to_owned(), "int4".to_owned())]);
    }

    #[test]
    fn text_read_equality_falls_back_with_a_warning_and_fetches_its_column() {
        let plan = plan(
            &columns(),
            Some(&["id".to_owned()]),
            &[request("amount", "=", json!("1.50"))],
        )
        .expect("plan");
        assert!(plan.pushed.is_empty());
        assert_eq!(plan.residual, vec![0]);
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].starts_with(FILTER_NOT_PUSHED));
        assert!(plan.fetch.iter().any(|(name, _)| name == "amount"));
    }

    #[test]
    fn ordered_filters_on_text_read_columns_are_refused() {
        let error =
            plan(&columns(), None, &[request("amount", "<", json!("2"))]).expect_err("refused");
        assert!(matches!(
            error,
            DowaiError::DatasetAccess {
                code: DATASET_FILTER_UNSUPPORTED,
                ..
            }
        ));
    }

    #[test]
    fn unknown_columns_operators_and_type_mismatches_are_invalid() {
        for bad in [
            request("missing", "=", json!(1)),
            request("id", "like", json!(1)),
            request("id", "=", json!("1")),
            request("id", "=", json!(null)),
            request("id", "is_null", json!(1)),
        ] {
            let error = plan(&columns(), None, &[bad]).expect_err("invalid");
            assert!(matches!(
                error,
                DowaiError::DatasetAccess {
                    code: DATASET_INVALID_QUERY,
                    ..
                }
            ));
        }
    }

    #[test]
    fn projection_must_name_existing_distinct_columns() {
        let unknown = plan(&columns(), Some(&["nope".to_owned()]), &[]);
        assert!(unknown.is_err());
        let duplicated = plan(&columns(), Some(&["id".to_owned(), "id".to_owned()]), &[]);
        assert!(duplicated.is_err());
        let empty = plan(&columns(), Some(&[]), &[]);
        assert!(empty.is_err());
    }
}
