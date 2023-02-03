//! This module contains code that implements
//! a gap-filling extension to DataFusion

use std::{
    fmt::{self, Debug},
    ops::{Bound, Range},
    sync::Arc,
};

use arrow::{compute::SortOptions, datatypes::SchemaRef};
use datafusion::{
    common::DFSchemaRef,
    error::{DataFusionError, Result},
    execution::context::TaskContext,
    logical_expr::{LogicalPlan, UserDefinedLogicalNode},
    physical_expr::{create_physical_expr, execution_props::ExecutionProps, PhysicalSortExpr},
    physical_plan::{
        expressions::Column, DisplayFormatType, Distribution, ExecutionPlan, Partitioning,
        PhysicalExpr, SendableRecordBatchStream, Statistics,
    },
    prelude::Expr,
};

/// A logical node that represents the gap filling operation.
#[derive(Clone, Debug)]
pub struct GapFill {
    input: Arc<LogicalPlan>,
    group_expr: Vec<Expr>,
    aggr_expr: Vec<Expr>,
    params: GapFillParams,
}

/// Parameters to the GapFill operation
#[derive(Clone, Debug)]
pub(crate) struct GapFillParams {
    /// The stride argument from the call to DATE_BIN_GAPFILL
    pub stride: Expr,
    /// The source time column
    pub time_column: Expr,
    /// The time range of the time column inferred from predicates
    /// in overall the query
    pub time_range: Range<Bound<Expr>>,
}

impl GapFill {
    pub(crate) fn try_new(
        input: Arc<LogicalPlan>,
        group_expr: Vec<Expr>,
        aggr_expr: Vec<Expr>,
        params: GapFillParams,
    ) -> Result<Self> {
        Ok(Self {
            input,
            group_expr,
            aggr_expr,
            params,
        })
    }
}

impl UserDefinedLogicalNode for GapFill {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![self.input.as_ref()]
    }

    fn schema(&self) -> &DFSchemaRef {
        self.input.schema()
    }

    fn expressions(&self) -> Vec<Expr> {
        self.group_expr
            .iter()
            .chain(self.aggr_expr.iter())
            .cloned()
            .collect()
    }

    fn fmt_for_explain(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GapFill: groupBy=[{:?}], aggr=[{:?}], time_column={}, stride={}, range={:?}",
            self.group_expr,
            self.aggr_expr,
            self.params.time_column,
            self.params.stride,
            self.params.time_range,
        )
    }

    fn from_template(
        &self,
        exprs: &[Expr],
        inputs: &[LogicalPlan],
    ) -> Arc<dyn UserDefinedLogicalNode> {
        let mut group_expr: Vec<_> = exprs.to_vec();
        let aggr_expr = group_expr.split_off(self.group_expr.len());
        let gapfill = Self::try_new(
            Arc::new(inputs[0].clone()),
            group_expr,
            aggr_expr,
            self.params.clone(),
        )
        .expect("should not fail");
        Arc::new(gapfill)
    }
}

/// Called by the extension planner to plan a [GapFill] node.
pub(crate) fn plan_gap_fill(
    execution_props: &ExecutionProps,
    gap_fill: &GapFill,
    logical_inputs: &[&LogicalPlan],
    physical_inputs: &[Arc<dyn ExecutionPlan>],
) -> Result<GapFillExec> {
    if logical_inputs.len() != 1 {
        return Err(DataFusionError::Internal(
            "GapFillExec: wrong number of logical inputs".to_string(),
        ));
    }
    if physical_inputs.len() != 1 {
        return Err(DataFusionError::Internal(
            "GapFillExec: wrong number of physical inputs".to_string(),
        ));
    }

    let input_dfschema = logical_inputs[0].schema().as_ref();
    let input_schema = physical_inputs[0].schema();
    let input_schema = input_schema.as_ref();

    let group_expr: Result<Vec<_>> = gap_fill
        .group_expr
        .iter()
        .map(|e| create_physical_expr(e, input_dfschema, input_schema, execution_props))
        .collect();
    let group_expr = group_expr?;

    let aggr_expr: Result<Vec<_>> = gap_fill
        .aggr_expr
        .iter()
        .map(|e| create_physical_expr(e, input_dfschema, input_schema, execution_props))
        .collect();
    let aggr_expr = aggr_expr?;

    let logical_time_column = gap_fill.params.time_column.try_into_col()?;
    let time_column = Column::new_with_schema(&logical_time_column.name, input_schema)?;

    let stride = create_physical_expr(
        &gap_fill.params.stride,
        input_dfschema,
        input_schema,
        execution_props,
    )?;

    let time_range = &gap_fill.params.time_range;
    let time_range = try_map_range(time_range, |b| {
        try_map_bound(b.as_ref(), |e| {
            create_physical_expr(e, input_dfschema, input_schema, execution_props)
        })
    })?;

    let params = GapFillExecParams {
        stride,
        time_column,
        time_range,
    };
    GapFillExec::try_new(
        Arc::clone(&physical_inputs[0]),
        group_expr,
        aggr_expr,
        params,
    )
}

fn try_map_range<T, U, F>(tr: &Range<T>, f: F) -> Result<Range<U>>
where
    F: Fn(&T) -> Result<U>,
{
    Ok(Range {
        start: f(&tr.start)?,
        end: f(&tr.end)?,
    })
}

fn try_map_bound<T, U, F>(bt: Bound<T>, f: F) -> Result<Bound<U>>
where
    F: FnOnce(T) -> Result<U>,
{
    Ok(match bt {
        Bound::Excluded(t) => Bound::Excluded(f(t)?),
        Bound::Included(t) => Bound::Included(f(t)?),
        Bound::Unbounded => Bound::Unbounded,
    })
}

/// A physical node for the gap-fill operation.
pub struct GapFillExec {
    input: Arc<dyn ExecutionPlan>,
    // The group by expressions from the original aggregation node.
    group_expr: Vec<Arc<dyn PhysicalExpr>>,
    // The aggregate expressions from the original aggregation node.
    aggr_expr: Vec<Arc<dyn PhysicalExpr>>,
    // The sort expressions for the required sort order of the input:
    // all of the group exressions, with the time column being last.
    sort_expr: Vec<PhysicalSortExpr>,
    // Parameters (besides streaming data) to gap filling
    params: GapFillExecParams,
}

#[derive(Clone, Debug)]
struct GapFillExecParams {
    /// The uniform interval of incoming timestamps
    stride: Arc<dyn PhysicalExpr>,
    /// The timestamp column produced by date_bin
    time_column: Column,
    /// The time range of timestamps in the time column
    time_range: Range<Bound<Arc<dyn PhysicalExpr>>>,
}

impl GapFillExec {
    fn try_new(
        input: Arc<dyn ExecutionPlan>,
        group_expr: Vec<Arc<dyn PhysicalExpr>>,
        aggr_expr: Vec<Arc<dyn PhysicalExpr>>,
        params: GapFillExecParams,
    ) -> Result<Self> {
        let sort_expr = {
            let mut sort_expr: Vec<_> = group_expr
                .iter()
                .map(|expr| PhysicalSortExpr {
                    expr: Arc::clone(expr),
                    options: SortOptions::default(),
                })
                .collect();

            // Ensure that the time column is the last component in the sort
            // expressions.
            let time_idx = group_expr
                .iter()
                .enumerate()
                .find(|(_i, e)| {
                    if let Some(col) = e.as_any().downcast_ref::<Column>() {
                        col.index() == params.time_column.index()
                    } else {
                        false
                    }
                })
                .map(|(i, _)| i);

            if let Some(time_idx) = time_idx {
                let last_elem = sort_expr.len() - 1;
                sort_expr.swap(time_idx, last_elem);
            } else {
                return Err(DataFusionError::Internal(
                    "could not find time column for GapFillExec".to_string(),
                ));
            }

            sort_expr
        };

        Ok(Self {
            input,
            group_expr,
            aggr_expr,
            sort_expr,
            params,
        })
    }
}

impl Debug for GapFillExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GapFillExec")
    }
}

impl ExecutionPlan for GapFillExec {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(1)
    }

    fn required_input_distribution(&self) -> Vec<Distribution> {
        // It seems like it could be possible to partition on all the
        // group keys except for the time expression. For now, keep it simple.
        vec![Distribution::SinglePartition]
    }

    fn output_ordering(&self) -> Option<&[datafusion::physical_expr::PhysicalSortExpr]> {
        self.input.output_ordering()
    }

    fn required_input_ordering(&self) -> Vec<Option<&[PhysicalSortExpr]>> {
        vec![Some(&self.sort_expr)]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true]
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![Arc::clone(&self.input)]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match children.len() {
            1 => Ok(Arc::new(Self::try_new(
                Arc::clone(&children[0]),
                self.group_expr.clone(),
                self.aggr_expr.clone(),
                self.params.clone(),
            )?)),
            _ => Err(DataFusionError::Internal(
                "GapFillExec wrong number of children".to_string(),
            )),
        }
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if self.output_partitioning().partition_count() <= partition {
            return Err(DataFusionError::Internal(format!(
                "GapFillExec invalid partition {partition}"
            )));
        }
        Err(DataFusionError::NotImplemented("gap filling".to_string()))
    }

    fn fmt_as(&self, t: DisplayFormatType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match t {
            DisplayFormatType::Default => {
                let group_expr: Vec<_> = self.group_expr.iter().map(|e| e.to_string()).collect();
                let aggr_expr: Vec<_> = self.aggr_expr.iter().map(|e| e.to_string()).collect();
                let time_range = try_map_range(&self.params.time_range, |b| {
                    try_map_bound(b.as_ref(), |e| Ok(e.to_string()))
                })
                .map_err(|_| fmt::Error {})?;
                write!(
                    f,
                    "GapFillExec: group_expr=[{}], aggr_expr=[{}], stride={}, time_range={:?}",
                    group_expr.join(", "),
                    aggr_expr.join(", "),
                    self.params.stride,
                    time_range
                )
            }
        }
    }

    fn statistics(&self) -> Statistics {
        Statistics::default()
    }
}

#[cfg(test)]
mod test {
    use std::ops::{Bound, Range};

    use crate::exec::{Executor, ExecutorType};

    use super::*;
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use datafusion::{
        datasource::empty::EmptyTable,
        error::Result,
        logical_expr::{logical_plan, Extension},
        physical_plan::displayable,
        prelude::{col, lit, lit_timestamp_nano},
        scalar::ScalarValue,
        sql::TableReference,
    };

    fn schema() -> Schema {
        Schema::new(vec![
            Field::new(
                "time",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            ),
            Field::new("loc", DataType::Utf8, false),
            Field::new("temp", DataType::Float64, false),
        ])
    }

    fn table_scan() -> Result<LogicalPlan> {
        let schema = schema();
        logical_plan::table_scan(Some("temps"), &schema, None)?.build()
    }

    #[test]
    fn fmt_logical_plan() -> Result<()> {
        // This test case does not make much sense but
        // just verifies we can construct a logical gapfill node
        // and show its plan.
        let scan = table_scan()?;
        let gapfill = GapFill::try_new(
            Arc::new(scan),
            vec![col("loc"), col("time")],
            vec![col("temp")],
            GapFillParams {
                stride: lit(ScalarValue::IntervalDayTime(Some(60_000))),
                time_column: col("time"),
                time_range: Range {
                    start: Bound::Included(lit_timestamp_nano(1000)),
                    end: Bound::Excluded(lit_timestamp_nano(2000)),
                },
            },
        )?;
        let plan = LogicalPlan::Extension(Extension {
            node: Arc::new(gapfill),
        });
        let expected = "GapFill: groupBy=[[loc, time]], aggr=[[temp]], time_column=time, stride=IntervalDayTime(\"60000\"), range=Included(TimestampNanosecond(1000, None))..Excluded(TimestampNanosecond(2000, None))\
                      \n  TableScan: temps";
        assert_eq!(expected, format!("{}", plan.display_indent()));
        Ok(())
    }

    async fn assert_explain(sql: &str, expected: &str) -> Result<()> {
        let executor = Executor::new_testing();
        let context = executor.new_context(ExecutorType::Query);
        context.inner().register_table(
            TableReference::Bare { table: "temps" },
            Arc::new(EmptyTable::new(Arc::new(schema()))),
        )?;
        let physical_plan = context.prepare_sql(sql).await?;
        let actual_plan = displayable(physical_plan.as_ref()).indent().to_string();
        let actual_iter = actual_plan.split('\n');

        let expected = expected.split('\n');
        expected.zip(actual_iter).for_each(|(expected, actual)| {
            assert_eq!(expected, actual, "\ncomplete plan was:\n{actual_plan:?}\n")
        });
        Ok(())
    }

    #[tokio::test]
    async fn plan_gap_fill() -> Result<()> {
        // show that the optimizer rule can fire and that physical
        // planning will succeed.
        let dbg_args = "IntervalDayTime(\"60000\"),temps.time,Utf8(\"1970-01-01T00:00:00Z\")";
        assert_explain(
            "SELECT date_bin_gapfill(interval '1 minute', time, timestamp '1970-01-01T00:00:00Z') AS minute, avg(temp)\
           \nFROM temps\
           \nWHERE time >= '1980-01-01T00:00:00Z' and time < '1981-01-01T00:00:00Z'
           \nGROUP BY minute;",
            format!(
                "ProjectionExec: expr=[date_bin_gapfill({dbg_args})@0 as minute, AVG(temps.temp)@1 as AVG(temps.temp)]\
               \n  GapFillExec: group_expr=[date_bin_gapfill({dbg_args})@0], aggr_expr=[AVG(temps.temp)@1], stride=60000, time_range=Included(\"315532800000000000\")..Excluded(\"347155200000000000\")\
               \n    SortExec: [date_bin_gapfill({dbg_args})@0 ASC]\
               \n      AggregateExec: mode=Final, gby=[date_bin_gapfill({dbg_args})@0 as date_bin_gapfill({dbg_args})], aggr=[AVG(temps.temp)]"
           ).as_str()
       ).await?;
        Ok(())
    }

    #[tokio::test]
    async fn gap_fill_exec_sort_order() -> Result<()> {
        // The call to `date_bin_gapfill` should be last in the SortExec
        // expressions, even though it was not last on the SELECT list
        // or the GROUP BY clause.
        let dbg_args = "IntervalDayTime(\"60000\"),temps.time,Utf8(\"1970-01-01T00:00:00Z\")";
        assert_explain(
            "SELECT \
           \n  loc,\
           \n  date_bin_gapfill(interval '1 minute', time, timestamp '1970-01-01T00:00:00Z') AS minute,\
           \n  concat('zz', loc) AS loczz,\
           \n  avg(temp)\
           \nFROM temps\
           \nWHERE time >= '1980-01-01T00:00:00Z' and time < '1981-01-01T00:00:00Z'
           \nGROUP BY loc, minute, loczz;",
            format!(
                "ProjectionExec: expr=[loc@0 as loc, date_bin_gapfill({dbg_args})@1 as minute, concat(Utf8(\"zz\"),temps.loc)@2 as loczz, AVG(temps.temp)@3 as AVG(temps.temp)]\
               \n  GapFillExec: group_expr=[loc@0, date_bin_gapfill({dbg_args})@1, concat(Utf8(\"zz\"),temps.loc)@2], aggr_expr=[AVG(temps.temp)@3], stride=60000, time_range=Included(\"315532800000000000\")..Excluded(\"347155200000000000\")\
               \n    SortExec: [loc@0 ASC,concat(Utf8(\"zz\"),temps.loc)@2 ASC,date_bin_gapfill({dbg_args})@1 ASC]\
               \n      AggregateExec: mode=Final, gby=[loc@0 as loc, date_bin_gapfill({dbg_args})@1 as date_bin_gapfill({dbg_args}), concat(Utf8(\"zz\"),temps.loc)@2 as concat(Utf8(\"zz\"),temps.loc)], aggr=[AVG(temps.temp)]"
           ).as_str()

           ).await?;
        Ok(())
    }
}
