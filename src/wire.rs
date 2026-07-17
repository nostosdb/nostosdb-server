use std::collections::BTreeMap;

use nostos_engine::{Parameters, QueryResult, QueryValue, StatementResult, WriteResult};
use serde::Serialize;
use serde_json::{Map, Value, json};

pub(crate) fn parameters(values: BTreeMap<String, Value>) -> Result<Parameters, String> {
    values
        .into_iter()
        .map(|(name, value)| Ok((name, query_value(value)?)))
        .collect()
}

fn query_value(value: Value) -> Result<QueryValue, String> {
    match value {
        Value::Null => Ok(QueryValue::Null),
        Value::Bool(value) => Ok(QueryValue::Boolean(value)),
        Value::Number(value) => value
            .as_i64()
            .map(QueryValue::Integer)
            .or_else(|| value.as_f64().map(QueryValue::Float))
            .ok_or_else(|| "parameter number is outside the supported range".to_owned()),
        Value::String(value) => Ok(QueryValue::String(value)),
        Value::Array(values) => values
            .into_iter()
            .map(query_value)
            .collect::<Result<Vec<_>, _>>()
            .map(QueryValue::List),
        Value::Object(values) => values
            .into_iter()
            .map(|(name, value)| Ok((name, query_value(value)?)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(QueryValue::Map),
    }
}

pub(crate) fn statement(value: &StatementResult) -> Value {
    match value {
        StatementResult::Read(result) => json!({
            "kind": "read",
            "result": query_result(result),
        }),
        StatementResult::Write(result) => json!({
            "kind": "write",
            "write": write_result(result),
        }),
    }
}

pub(crate) fn query_result(result: &QueryResult) -> Value {
    json!({
        "columns": result.columns,
        "rows": result.rows.iter().map(|row| {
            row.iter().map(to_json).collect::<Vec<_>>()
        }).collect::<Vec<_>>(),
        "ordered": result.ordered,
    })
}

fn write_result(result: &WriteResult) -> Value {
    json!({
        "summary": {
            "nodes_created": result.summary.nodes_created,
            "edges_created": result.summary.edges_created,
            "nodes_deleted": result.summary.nodes_deleted,
            "edges_deleted": result.summary.edges_deleted,
            "properties_set": result.summary.properties_set,
            "properties_removed": result.summary.properties_removed,
        },
        "result": result.result.as_ref().map(query_result),
    })
}

pub(crate) fn row_object(columns: &[String], row: &[QueryValue]) -> Value {
    Value::Object(
        columns
            .iter()
            .enumerate()
            .map(|(index, column)| (column.clone(), row.get(index).map_or(Value::Null, to_json)))
            .collect(),
    )
}

fn to_json(value: &QueryValue) -> Value {
    match value {
        QueryValue::Null => Value::Null,
        QueryValue::Boolean(value) => Value::Bool(*value),
        QueryValue::Integer(value) => json!(value),
        QueryValue::Float(value) => json!(value),
        QueryValue::String(value) => Value::String(value.clone()),
        QueryValue::Bytes(value) => Value::String(hex(value)),
        QueryValue::Duration(value) => json!({"nanoseconds": value}),
        QueryValue::List(values) => Value::Array(values.iter().map(to_json).collect()),
        QueryValue::Map(values) => Value::Object(
            values
                .iter()
                .map(|(name, value)| (name.clone(), to_json(value)))
                .collect(),
        ),
        QueryValue::Node(node) => json!({
            "id": node.id.get(),
            "state": format!("{:?}", node.state).to_ascii_lowercase(),
            "labels": node.labels,
            "properties": node.properties.iter().map(|(name, value)| {
                (name.clone(), to_json(value))
            }).collect::<Map<_, _>>(),
        }),
        QueryValue::Edge(edge) => json!({
            "id": edge.id.get(),
            "kind": format!("{:?}", edge.kind).to_ascii_lowercase(),
            "source": edge.source.get(),
            "target": edge.target.get(),
            "type": edge.relationship_type,
            "properties": edge.properties.iter().map(|(name, value)| {
                (name.clone(), to_json(value))
            }).collect::<Map<_, _>>(),
        }),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Serialize)]
pub(crate) struct ErrorBody<'a> {
    pub(crate) error: ErrorDetail<'a>,
}

#[derive(Serialize)]
pub(crate) struct ErrorDetail<'a> {
    pub(crate) code: &'a str,
    pub(crate) message: String,
}
