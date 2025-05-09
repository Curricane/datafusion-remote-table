use crate::connection::{RemoteDbType, projections_contains};
use crate::{
    Connection, ConnectionOptions, DFResult, Pool, RemoteField, RemoteSchema, RemoteSchemaRef,
    RemoteType, SqliteType,
};
use datafusion::arrow::array::{
    ArrayBuilder, ArrayRef, BinaryBuilder, Float64Builder, Int64Builder, NullBuilder, RecordBatch,
    StringBuilder, make_builder,
};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::common::{DataFusionError, project_schema};
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::prelude::Expr;
use derive_getters::Getters;
use derive_with::With;
use itertools::Itertools;
use rusqlite::types::ValueRef;
use rusqlite::{Column, Row, Rows};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, With, Getters)]
pub struct SqliteConnectionOptions {
    pub path: PathBuf,
    pub stream_chunk_size: usize,
}

impl SqliteConnectionOptions {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            stream_chunk_size: 2048,
        }
    }
}

#[derive(Debug)]
pub struct SqlitePool {
    pool: tokio_rusqlite::Connection,
}

pub async fn connect_sqlite(options: &SqliteConnectionOptions) -> DFResult<SqlitePool> {
    let pool = tokio_rusqlite::Connection::open(&options.path)
        .await
        .map_err(|e| {
            DataFusionError::Execution(format!("Failed to open sqlite connection: {e:?}"))
        })?;
    Ok(SqlitePool { pool })
}

#[async_trait::async_trait]
impl Pool for SqlitePool {
    async fn get(&self) -> DFResult<Arc<dyn Connection>> {
        let conn = self.pool.clone();
        Ok(Arc::new(SqliteConnection { conn }))
    }
}

#[derive(Debug)]
pub struct SqliteConnection {
    conn: tokio_rusqlite::Connection,
}

#[async_trait::async_trait]
impl Connection for SqliteConnection {
    async fn infer_schema(&self, sql: &str) -> DFResult<(RemoteSchemaRef, SchemaRef)> {
        let sql = sql.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let columns: Vec<OwnedColumn> =
                    stmt.columns().iter().map(sqlite_col_to_owned_col).collect();
                let rows = stmt.query([])?;

                let remote_schema = Arc::new(
                    build_remote_schema(columns.as_slice(), rows)
                        .map_err(|e| tokio_rusqlite::Error::Other(Box::new(e)))?,
                );
                let arrow_schema = Arc::new(remote_schema.to_arrow_schema());
                Ok((remote_schema, arrow_schema))
            })
            .await
            .map_err(|e| DataFusionError::Execution(format!("Failed to infer schema: {e:?}")))
    }

    async fn query(
        &self,
        conn_options: &ConnectionOptions,
        sql: &str,
        table_schema: SchemaRef,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<SendableRecordBatchStream> {
        let projected_schema = project_schema(&table_schema, projection)?;
        let sql = RemoteDbType::Sqlite
            .try_rewrite_query(sql, filters, limit)
            .unwrap_or_else(|| sql.to_string());
        let conn = self.conn.clone();
        let projection = projection.cloned();
        let limit = conn_options.stream_chunk_size();
        let stream = async_stream::stream! {
            let mut offset = 0;
            loop {
                let sql = format!("SELECT * FROM ({sql}) LIMIT {limit} OFFSET {offset}");
                let sql_clone = sql.clone();
                let conn = conn.clone();
                let projection = projection.clone();
                let table_schema = table_schema.clone();
                let (batch, is_empty) = conn
                    .call(move |conn| {
                        let mut stmt = conn.prepare(&sql)?;
                        let columns: Vec<OwnedColumn> =
                            stmt.columns().iter().map(sqlite_col_to_owned_col).collect();
                        let rows = stmt.query([])?;

                        rows_to_batch(rows, &table_schema, columns, projection.as_ref())
                            .map_err(|e| tokio_rusqlite::Error::Other(e.into()))
                })
                .await
                .map_err(|e| {
                    DataFusionError::Execution(format!(
                        "Failed to execute query {sql_clone} on sqlite: {e:?}"
                    ))
                })?;
                if is_empty {
                    break;
                }
                yield Ok(batch);
                offset += limit;
            }
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            projected_schema,
            stream,
        )))
    }
}

#[derive(Debug)]
struct OwnedColumn {
    name: String,
    decl_type: Option<String>,
}

fn sqlite_col_to_owned_col(sqlite_col: &Column) -> OwnedColumn {
    OwnedColumn {
        name: sqlite_col.name().to_string(),
        decl_type: sqlite_col.decl_type().map(|x| x.to_string()),
    }
}

// TODO return sqlite type
fn decl_type_to_remote_type(decl_type: &str) -> DFResult<RemoteType> {
    if ["tinyint", "smallint", "int", "integer", "bigint"].contains(&decl_type) {
        return Ok(RemoteType::Sqlite(SqliteType::Integer));
    }
    if ["real", "float", "double"].contains(&decl_type) {
        return Ok(RemoteType::Sqlite(SqliteType::Real));
    }
    if decl_type.starts_with("real") {
        return Ok(RemoteType::Sqlite(SqliteType::Real));
    }
    if ["text", "varchar", "char", "string"].contains(&decl_type) {
        return Ok(RemoteType::Sqlite(SqliteType::Text));
    }
    if decl_type.starts_with("char")
        || decl_type.starts_with("varchar")
        || decl_type.starts_with("text")
    {
        return Ok(RemoteType::Sqlite(SqliteType::Text));
    }
    if ["binary", "varbinary", "tinyblob", "blob"].contains(&decl_type) {
        return Ok(RemoteType::Sqlite(SqliteType::Blob));
    }
    if decl_type.starts_with("binary") || decl_type.starts_with("varbinary") {
        return Ok(RemoteType::Sqlite(SqliteType::Blob));
    }
    Err(DataFusionError::NotImplemented(format!(
        "Unsupported sqlite decl type: {decl_type}",
    )))
}

fn build_remote_schema(columns: &[OwnedColumn], mut rows: Rows) -> DFResult<RemoteSchema> {
    let mut remote_field_map = HashMap::with_capacity(columns.len());
    let mut unknown_cols = vec![];
    for (col_idx, col) in columns.iter().enumerate() {
        if let Some(decl_type) = &col.decl_type {
            let remote_type = decl_type_to_remote_type(&decl_type.to_ascii_lowercase())?;
            remote_field_map.insert(col_idx, RemoteField::new(&col.name, remote_type, true));
        } else {
            unknown_cols.push(col_idx);
        }
    }

    if !unknown_cols.is_empty() {
        while let Some(row) = rows.next().map_err(|e| {
            DataFusionError::Execution(format!("Failed to get next row from sqlite: {e:?}"))
        })? {
            let mut to_be_removed = vec![];
            for col_idx in unknown_cols.iter() {
                let value_ref = row.get_ref(*col_idx).map_err(|e| {
                    DataFusionError::Execution(format!(
                        "Failed to get value ref for column {col_idx}: {e:?}"
                    ))
                })?;
                match value_ref {
                    ValueRef::Null => {}
                    ValueRef::Integer(_) => {
                        remote_field_map.insert(
                            *col_idx,
                            RemoteField::new(
                                columns[*col_idx].name.clone(),
                                RemoteType::Sqlite(SqliteType::Integer),
                                true,
                            ),
                        );
                        to_be_removed.push(*col_idx);
                    }
                    ValueRef::Real(_) => {
                        remote_field_map.insert(
                            *col_idx,
                            RemoteField::new(
                                columns[*col_idx].name.clone(),
                                RemoteType::Sqlite(SqliteType::Real),
                                true,
                            ),
                        );
                        to_be_removed.push(*col_idx);
                    }
                    ValueRef::Text(_) => {
                        remote_field_map.insert(
                            *col_idx,
                            RemoteField::new(
                                columns[*col_idx].name.clone(),
                                RemoteType::Sqlite(SqliteType::Text),
                                true,
                            ),
                        );
                        to_be_removed.push(*col_idx);
                    }
                    ValueRef::Blob(_) => {
                        remote_field_map.insert(
                            *col_idx,
                            RemoteField::new(
                                columns[*col_idx].name.clone(),
                                RemoteType::Sqlite(SqliteType::Blob),
                                true,
                            ),
                        );
                        to_be_removed.push(*col_idx);
                    }
                }
            }
            for col_idx in to_be_removed.iter() {
                unknown_cols.retain(|&x| x != *col_idx);
            }
            if unknown_cols.is_empty() {
                break;
            }
        }
    }

    if !unknown_cols.is_empty() {
        return Err(DataFusionError::NotImplemented(format!(
            "Failed to infer sqlite decl type for columns: {unknown_cols:?}"
        )));
    }
    let remote_fields = remote_field_map
        .into_iter()
        .sorted_by_key(|entry| entry.0)
        .map(|entry| entry.1)
        .collect::<Vec<_>>();
    Ok(RemoteSchema::new(remote_fields))
}

fn rows_to_batch(
    mut rows: Rows,
    table_schema: &SchemaRef,
    columns: Vec<OwnedColumn>,
    projection: Option<&Vec<usize>>,
) -> DFResult<(RecordBatch, bool)> {
    let projected_schema = project_schema(table_schema, projection)?;
    let mut array_builders = vec![];
    for field in table_schema.fields() {
        let builder = make_builder(field.data_type(), 1000);
        array_builders.push(builder);
    }

    let mut is_empty = true;
    while let Some(row) = rows.next().map_err(|e| {
        DataFusionError::Execution(format!("Failed to get next row from sqlite: {e:?}"))
    })? {
        is_empty = false;
        append_rows_to_array_builders(
            row,
            table_schema,
            &columns,
            projection,
            array_builders.as_mut_slice(),
        )?;
    }

    let projected_columns = array_builders
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| projections_contains(projection, *idx))
        .map(|(_, mut builder)| builder.finish())
        .collect::<Vec<ArrayRef>>();
    Ok((
        RecordBatch::try_new(projected_schema, projected_columns)?,
        is_empty,
    ))
}

macro_rules! handle_primitive_type {
    ($builder:expr, $field:expr, $col:expr, $builder_ty:ty, $value_ty:ty, $row:expr, $index:expr) => {{
        let builder = $builder
            .as_any_mut()
            .downcast_mut::<$builder_ty>()
            .unwrap_or_else(|| {
                panic!(
                    "Failed to downcast builder to {} for {:?} and {:?}",
                    stringify!($builder_ty),
                    $field,
                    $col
                )
            });

        let v: Option<$value_ty> = $row.get($index).map_err(|e| {
            DataFusionError::Execution(format!(
                "Failed to get optional {} value for {:?} and {:?}: {e:?}",
                stringify!($value_ty),
                $field,
                $col
            ))
        })?;

        match v {
            Some(v) => builder.append_value(v),
            None => builder.append_null(),
        }
    }};
}

fn append_rows_to_array_builders(
    row: &Row,
    table_schema: &SchemaRef,
    columns: &[OwnedColumn],
    projection: Option<&Vec<usize>>,
    array_builders: &mut [Box<dyn ArrayBuilder>],
) -> DFResult<()> {
    for (idx, field) in table_schema.fields.iter().enumerate() {
        if !projections_contains(projection, idx) {
            continue;
        }
        let builder = &mut array_builders[idx];
        let col = columns.get(idx);
        match field.data_type() {
            DataType::Null => {
                let builder = builder
                    .as_any_mut()
                    .downcast_mut::<NullBuilder>()
                    .expect("Failed to downcast builder to NullBuilder");
                builder.append_null();
            }
            DataType::Int64 => {
                handle_primitive_type!(builder, field, col, Int64Builder, i64, row, idx);
            }
            DataType::Float64 => {
                handle_primitive_type!(builder, field, col, Float64Builder, f64, row, idx);
            }
            DataType::Utf8 => {
                handle_primitive_type!(builder, field, col, StringBuilder, String, row, idx);
            }
            DataType::Binary => {
                handle_primitive_type!(builder, field, col, BinaryBuilder, Vec<u8>, row, idx);
            }
            _ => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported data type {:?} for col: {:?}",
                    field.data_type(),
                    col
                )));
            }
        }
    }
    Ok(())
}
