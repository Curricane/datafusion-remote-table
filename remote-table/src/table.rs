use crate::{
    ConnectionOptions, DFResult, DefaultTransform, DefaultUnparser, Pool, RemoteSchemaRef,
    RemoteTableExec, Transform, Unparse, connect, transform_schema,
};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::{Session, TableProvider};
use datafusion::common::Column;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::datasource::TableType;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_plan::ExecutionPlan;
use std::any::Any;
use std::sync::Arc;

#[derive(Debug)]
pub struct RemoteTable {
    pub(crate) conn_options: ConnectionOptions,
    pub(crate) sql: String,
    pub(crate) table_schema: SchemaRef,
    pub(crate) transformed_table_schema: SchemaRef,
    pub(crate) remote_schema: Option<RemoteSchemaRef>,
    pub(crate) transform: Arc<dyn Transform>,
    pub(crate) unparser: Arc<dyn Unparse>,
    pub(crate) pool: Arc<dyn Pool>,
}

impl RemoteTable {
    pub async fn try_new(
        conn_options: ConnectionOptions,
        sql: impl Into<String>,
    ) -> DFResult<Self> {
        Self::try_new_with_schema_transform_unparser(
            conn_options,
            sql,
            None,
            Arc::new(DefaultTransform {}),
            Arc::new(DefaultUnparser {}),
        )
        .await
    }

    pub async fn try_new_with_schema(
        conn_options: ConnectionOptions,
        sql: impl Into<String>,
        table_schema: SchemaRef,
    ) -> DFResult<Self> {
        Self::try_new_with_schema_transform_unparser(
            conn_options,
            sql,
            Some(table_schema),
            Arc::new(DefaultTransform {}),
            Arc::new(DefaultUnparser {}),
        )
        .await
    }

    pub async fn try_new_with_transform(
        conn_options: ConnectionOptions,
        sql: impl Into<String>,
        transform: Arc<dyn Transform>,
    ) -> DFResult<Self> {
        Self::try_new_with_schema_transform_unparser(
            conn_options,
            sql,
            None,
            transform,
            Arc::new(DefaultUnparser {}),
        )
        .await
    }

    pub async fn try_new_with_schema_transform_unparser(
        conn_options: ConnectionOptions,
        sql: impl Into<String>,
        table_schema: Option<SchemaRef>,
        transform: Arc<dyn Transform>,
        unparser: Arc<dyn Unparse>,
    ) -> DFResult<Self> {
        let sql = sql.into();
        let pool = connect(&conn_options).await?;

        let (table_schema, remote_schema) = if let Some(table_schema) = table_schema {
            let remote_schema = if transform.as_any().is::<DefaultTransform>() {
                None
            } else {
                // Infer remote schema
                let conn = pool.get().await?;
                conn.infer_schema(&sql).await.ok()
            };
            (table_schema, remote_schema)
        } else {
            // Infer table schema
            let conn = pool.get().await?;
            match conn.infer_schema(&sql).await {
                Ok(remote_schema) => {
                    let inferred_table_schema = Arc::new(remote_schema.to_arrow_schema());
                    (inferred_table_schema, Some(remote_schema))
                }
                Err(e) => {
                    return Err(DataFusionError::Execution(format!(
                        "Failed to infer schema: {e}"
                    )));
                }
            }
        };

        let transformed_table_schema = transform_schema(
            table_schema.clone(),
            transform.as_ref(),
            remote_schema.as_ref(),
        )?;

        Ok(RemoteTable {
            conn_options,
            sql,
            table_schema,
            transformed_table_schema,
            remote_schema,
            transform,
            unparser,
            pool,
        })
    }

    pub fn remote_schema(&self) -> Option<RemoteSchemaRef> {
        self.remote_schema.clone()
    }
}

#[async_trait::async_trait]
impl TableProvider for RemoteTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.transformed_table_schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        let transformed_table_schema = transform_schema(
            self.table_schema.clone(),
            self.transform.as_ref(),
            self.remote_schema.as_ref(),
        )?;
        let rewritten_filters = rewrite_filters_column(
            filters.to_vec(),
            &self.table_schema,
            &transformed_table_schema,
        )?;
        let mut unparsed_filters = vec![];
        for filter in rewritten_filters {
            unparsed_filters.push(
                self.unparser
                    .unparse_filter(&filter, self.conn_options.db_type())?,
            );
        }

        Ok(Arc::new(RemoteTableExec::try_new(
            self.conn_options.clone(),
            self.sql.clone(),
            self.table_schema.clone(),
            self.remote_schema.clone(),
            projection.cloned(),
            unparsed_filters,
            limit,
            self.transform.clone(),
            self.pool.get().await?,
        )?))
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DFResult<Vec<TableProviderFilterPushDown>> {
        if !self
            .conn_options
            .db_type()
            .support_rewrite_with_filters_limit(&self.sql)
        {
            return Ok(vec![
                TableProviderFilterPushDown::Unsupported;
                filters.len()
            ]);
        }
        let mut pushdown = vec![];
        for filter in filters {
            pushdown.push(
                self.unparser
                    .support_filter_pushdown(filter, self.conn_options.db_type())?,
            );
        }
        Ok(pushdown)
    }
}

pub(crate) fn rewrite_filters_column(
    filters: Vec<Expr>,
    table_schema: &SchemaRef,
    transformed_table_schema: &SchemaRef,
) -> DFResult<Vec<Expr>> {
    filters
        .into_iter()
        .map(|f| {
            f.transform_down(|e| {
                if let Expr::Column(col) = e {
                    let col_idx = transformed_table_schema.index_of(col.name())?;
                    let row_name = table_schema.field(col_idx).name().to_string();
                    Ok(Transformed::yes(Expr::Column(Column::new_unqualified(
                        row_name,
                    ))))
                } else {
                    Ok(Transformed::no(e))
                }
            })
            .map(|trans| trans.data)
        })
        .collect::<DFResult<Vec<_>>>()
}
