// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! [`CoalesceBatchesExec`] combines small batches into larger batches.

use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet};
use super::{DisplayAs, ExecutionPlanProperties, PlanProperties, Statistics};
use crate::{
    DisplayFormatType, ExecutionPlan, RecordBatchStream, SendableRecordBatchStream,
};

use arrow::array::{AsArray, StringViewBuilder};
use arrow::compute::concat_batches;
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use arrow_array::{Array, ArrayRef};
use datafusion_common::Result;
use datafusion_execution::TaskContext;

use futures::ready;
use futures::stream::{Stream, StreamExt};

/// `CoalesceBatchesExec` combines small batches into larger batches for more
/// efficient use of vectorized processing by later operators.
///
/// The operator buffers batches until it collects `target_batch_size` rows and
/// then emits a single concatenated batch. When only a limited number of rows
/// are necessary (specified by the `fetch` parameter), the operator will stop
/// buffering and returns the final batch once the number of collected rows
/// reaches the `fetch` value.
///
/// # Background
///
/// Generally speaking, larger RecordBatches are more efficient to process than
/// smaller record batches (until the CPU cache is exceeded) because there is
/// fixed processing overhead per batch. This code concatenates multiple small
/// record batches into larger ones to amortize this overhead.
///
/// ```text
/// ┌────────────────────┐
/// │    RecordBatch     │
/// │   num_rows = 23    │
/// └────────────────────┘                 ┌────────────────────┐
///                                        │                    │
/// ┌────────────────────┐     Coalesce    │                    │
/// │                    │      Batches    │                    │
/// │    RecordBatch     │                 │                    │
/// │   num_rows = 50    │  ─ ─ ─ ─ ─ ─ ▶  │                    │
/// │                    │                 │    RecordBatch     │
/// │                    │                 │   num_rows = 106   │
/// └────────────────────┘                 │                    │
///                                        │                    │
/// ┌────────────────────┐                 │                    │
/// │                    │                 │                    │
/// │    RecordBatch     │                 │                    │
/// │   num_rows = 33    │                 └────────────────────┘
/// │                    │
/// └────────────────────┘
/// ```

#[derive(Debug)]
pub struct CoalesceBatchesExec {
    /// The input plan
    input: Arc<dyn ExecutionPlan>,
    /// Minimum number of rows for coalesces batches
    target_batch_size: usize,
    /// Maximum number of rows to fetch, `None` means fetching all rows
    fetch: Option<usize>,
    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
    cache: PlanProperties,
}

impl CoalesceBatchesExec {
    /// Create a new CoalesceBatchesExec
    pub fn new(input: Arc<dyn ExecutionPlan>, target_batch_size: usize) -> Self {
        let cache = Self::compute_properties(&input);
        Self {
            input,
            target_batch_size,
            fetch: None,
            metrics: ExecutionPlanMetricsSet::new(),
            cache,
        }
    }

    /// Update fetch with the argument
    pub fn with_fetch(mut self, fetch: Option<usize>) -> Self {
        self.fetch = fetch;
        self
    }

    /// The input plan
    pub fn input(&self) -> &Arc<dyn ExecutionPlan> {
        &self.input
    }

    /// Minimum number of rows for coalesces batches
    pub fn target_batch_size(&self) -> usize {
        self.target_batch_size
    }

    /// This function creates the cache object that stores the plan properties such as schema, equivalence properties, ordering, partitioning, etc.
    fn compute_properties(input: &Arc<dyn ExecutionPlan>) -> PlanProperties {
        // The coalesce batches operator does not make any changes to the
        // partitioning of its input.
        PlanProperties::new(
            input.equivalence_properties().clone(), // Equivalence Properties
            input.output_partitioning().clone(),    // Output Partitioning
            input.execution_mode(),                 // Execution Mode
        )
    }
}

impl DisplayAs for CoalesceBatchesExec {
    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "CoalesceBatchesExec: target_batch_size={}",
                    self.target_batch_size,
                )?;
                if let Some(fetch) = self.fetch {
                    write!(f, ", fetch={fetch}")?;
                };

                Ok(())
            }
        }
    }
}

impl ExecutionPlan for CoalesceBatchesExec {
    fn name(&self) -> &'static str {
        "CoalesceBatchesExec"
    }

    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.cache
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true]
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(
            CoalesceBatchesExec::new(Arc::clone(&children[0]), self.target_batch_size)
                .with_fetch(self.fetch),
        ))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        Ok(Box::pin(CoalesceBatchesStream {
            input: self.input.execute(partition, context)?,
            coalescer: BatchCoalescer::new(
                self.input.schema(),
                self.target_batch_size,
                self.fetch,
            ),
            baseline_metrics: BaselineMetrics::new(&self.metrics, partition),
            // Start by pulling data
            inner_state: CoalesceBatchesStreamState::Pull,
        }))
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Result<Statistics> {
        Statistics::with_fetch(self.input.statistics()?, self.schema(), self.fetch, 0, 1)
    }

    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        Some(Arc::new(CoalesceBatchesExec {
            input: Arc::clone(&self.input),
            target_batch_size: self.target_batch_size,
            fetch: limit,
            metrics: self.metrics.clone(),
            cache: self.cache.clone(),
        }))
    }

    fn fetch(&self) -> Option<usize> {
        self.fetch
    }
}

/// Stream for [`CoalesceBatchesExec`]. See [`CoalesceBatchesExec`] for more details.
struct CoalesceBatchesStream {
    /// The input plan
    input: SendableRecordBatchStream,
    /// Buffer for combining batches
    coalescer: BatchCoalescer,
    /// Execution metrics
    baseline_metrics: BaselineMetrics,
    /// The current inner state of the stream. This state dictates the current
    /// action or operation to be performed in the streaming process.
    inner_state: CoalesceBatchesStreamState,
}

impl Stream for CoalesceBatchesStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let poll = self.poll_next_inner(cx);
        self.baseline_metrics.record_poll(poll)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // we can't predict the size of incoming batches so re-use the size hint from the input
        self.input.size_hint()
    }
}

/// Enumeration of possible states for `CoalesceBatchesStream`.
/// It represents different stages in the lifecycle of a stream of record batches.
///
/// An example of state transition:
/// Notation:
/// `[3000]`: A batch with size 3000
/// `{[2000], [3000]}`: `CoalesceBatchStream`'s internal buffer with 2 batches buffered
/// Input of `CoalesceBatchStream` will generate three batches `[2000], [3000], [4000]`
/// The coalescing procedure will go through the following steps with 4096 coalescing threshold:
/// 1. Read the first batch and get it buffered.
/// - initial state: `Pull`
/// - initial buffer: `{}`
/// - updated buffer: `{[2000]}`
/// - next state: `Pull`
/// 2. Read the second batch, the coalescing target is reached since 2000 + 3000 > 4096
/// - initial state: `Pull`
/// - initial buffer: `{[2000]}`
/// - updated buffer: `{[2000], [3000]}`
/// - next state: `ReturnBuffer`
/// 4. Two batches in the batch get merged and consumed by the upstream operator.
/// - initial state: `ReturnBuffer`
/// - initial buffer: `{[2000], [3000]}`
/// - updated buffer: `{}`
/// - next state: `Pull`
/// 5. Read the third input batch.
/// - initial state: `Pull`
/// - initial buffer: `{}`
/// - updated buffer: `{[4000]}`
/// - next state: `Pull`
/// 5. The input is ended now. Jump to exhaustion state preparing the finalized data.
/// - initial state: `Pull`
/// - initial buffer: `{[4000]}`
/// - updated buffer: `{[4000]}`
/// - next state: `Exhausted`
#[derive(Debug, Clone, Eq, PartialEq)]
enum CoalesceBatchesStreamState {
    /// State to pull a new batch from the input stream.
    Pull,
    /// State to return a buffered batch.
    ReturnBuffer,
    /// State indicating that the stream is exhausted.
    Exhausted,
}

impl CoalesceBatchesStream {
    fn poll_next_inner(
        self: &mut Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<RecordBatch>>> {
        let cloned_time = self.baseline_metrics.elapsed_compute().clone();
        loop {
            match &self.inner_state {
                CoalesceBatchesStreamState::Pull => {
                    // Attempt to pull the next batch from the input stream.
                    let input_batch = ready!(self.input.poll_next_unpin(cx));
                    // Start timing the operation. The timer records time upon being dropped.
                    let _timer = cloned_time.timer();

                    match input_batch {
                        Some(Ok(batch)) => match self.coalescer.push_batch(batch) {
                            CoalescerState::Continue => {}
                            CoalescerState::LimitReached => {
                                self.inner_state = CoalesceBatchesStreamState::Exhausted;
                            }
                            CoalescerState::TargetReached => {
                                self.inner_state =
                                    CoalesceBatchesStreamState::ReturnBuffer;
                            }
                        },
                        None => {
                            // End of input stream, but buffered batches might still be present.
                            self.inner_state = CoalesceBatchesStreamState::Exhausted;
                        }
                        other => return Poll::Ready(other),
                    }
                }
                CoalesceBatchesStreamState::ReturnBuffer => {
                    // Combine buffered batches into one batch and return it.
                    let batch = self.coalescer.finish_batch()?;
                    // Set to pull state for the next iteration.
                    self.inner_state = CoalesceBatchesStreamState::Pull;
                    return Poll::Ready(Some(Ok(batch)));
                }
                CoalesceBatchesStreamState::Exhausted => {
                    // Handle the end of the input stream.
                    return if self.coalescer.buffer.is_empty() {
                        // If buffer is empty, return None indicating the stream is fully consumed.
                        Poll::Ready(None)
                    } else {
                        // If the buffer still contains batches, prepare to return them.
                        let batch = self.coalescer.finish_batch()?;
                        Poll::Ready(Some(Ok(batch)))
                    };
                }
            }
        }
    }
}

impl RecordBatchStream for CoalesceBatchesStream {
    fn schema(&self) -> SchemaRef {
        self.coalescer.schema()
    }
}

/// Concatenate multiple record batches into larger batches
///
/// See [`CoalesceBatchesExec`] for more details.
///
/// Notes:
///
/// 1. The output rows is the same order as the input rows
///
/// 2. The output is a sequence of batches, with all but the last being at least
///    `target_batch_size` rows.
///
/// 3. Eventually this may also be able to handle other optimizations such as a
///    combined filter/coalesce operation.
#[derive(Debug)]
struct BatchCoalescer {
    /// The input schema
    schema: SchemaRef,
    /// Minimum number of rows for coalesces batches
    target_batch_size: usize,
    /// Total number of rows returned so far
    total_rows: usize,
    /// Buffered batches
    buffer: Vec<RecordBatch>,
    /// Buffered row count
    buffered_rows: usize,
    /// Maximum number of rows to fetch, `None` means fetching all rows
    fetch: Option<usize>,
}

impl BatchCoalescer {
    /// Create a new `BatchCoalescer`
    ///
    /// # Arguments
    /// - `schema` - the schema of the output batches
    /// - `target_batch_size` - the minimum number of rows for each
    ///    output batch (until limit reached)
    /// - `fetch` - the maximum number of rows to fetch, `None` means fetch all rows
    fn new(schema: SchemaRef, target_batch_size: usize, fetch: Option<usize>) -> Self {
        Self {
            schema,
            target_batch_size,
            total_rows: 0,
            buffer: vec![],
            buffered_rows: 0,
            fetch,
        }
    }

    /// Return the schema of the output batches
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Given a batch, it updates the buffer of [`BatchCoalescer`]. It returns
    /// a variant of [`CoalescerState`] indicating the final state of the buffer.
    fn push_batch(&mut self, batch: RecordBatch) -> CoalescerState {
        let batch = gc_string_view_batch(&batch);
        if self.limit_reached(&batch) {
            CoalescerState::LimitReached
        } else if self.target_reached(batch) {
            CoalescerState::TargetReached
        } else {
            CoalescerState::Continue
        }
    }

    /// The function checks if the buffer can reach the specified limit after getting `batch`.
    /// If it does, it slices the received batch as needed, updates the buffer with it, and
    /// finally returns `true`. Otherwise; the function does nothing and returns `false`.
    fn limit_reached(&mut self, batch: &RecordBatch) -> bool {
        match self.fetch {
            Some(fetch) if self.total_rows + batch.num_rows() >= fetch => {
                // Limit is reached
                let remaining_rows = fetch - self.total_rows;
                debug_assert!(remaining_rows > 0);

                let batch = batch.slice(0, remaining_rows);
                self.buffered_rows += batch.num_rows();
                self.total_rows = fetch;
                self.buffer.push(batch);
                true
            }
            _ => false,
        }
    }

    /// Updates the buffer with the given batch. If the target batch size is reached,
    /// the function returns `true`. Otherwise, it returns `false`.
    fn target_reached(&mut self, batch: RecordBatch) -> bool {
        if batch.num_rows() == 0 {
            false
        } else {
            self.total_rows += batch.num_rows();
            self.buffered_rows += batch.num_rows();
            self.buffer.push(batch);
            self.buffered_rows >= self.target_batch_size
        }
    }

    /// Concatenates and returns all buffered batches, and clears the buffer.
    fn finish_batch(&mut self) -> Result<RecordBatch> {
        let batch = concat_batches(&self.schema, &self.buffer)?;
        self.buffer.clear();
        self.buffered_rows = 0;
        Ok(batch)
    }
}

/// This enumeration acts as a status indicator for the [`BatchCoalescer`] after a
/// [`BatchCoalescer::push_batch()`] operation.
enum CoalescerState {
    /// Neither the limit nor the target batch size is reached.
    Continue,
    /// The sufficient row count to produce a complete query result is reached.
    LimitReached,
    /// The specified minimum number of rows a batch should have is reached.
    TargetReached,
}

/// Heuristically compact `StringViewArray`s to reduce memory usage, if needed
///
/// This function decides when to consolidate the StringView into a new buffer
/// to reduce memory usage and improve string locality for better performance.
///
/// This differs from `StringViewArray::gc` because:
/// 1. It may not compact the array depending on a heuristic.
/// 2. It uses a precise block size to reduce the number of buffers to track.
///
/// # Heuristic
///
/// If the average size of each view is larger than 32 bytes, we compact the array.
///
/// `StringViewArray` include pointers to buffer that hold the underlying data.
/// One of the great benefits of `StringViewArray` is that many operations
/// (e.g., `filter`) can be done without copying the underlying data.
///
/// However, after a while (e.g., after `FilterExec` or `HashJoinExec`) the
/// `StringViewArray` may only refer to a small portion of the buffer,
/// significantly increasing memory usage.
fn gc_string_view_batch(batch: &RecordBatch) -> RecordBatch {
    let new_columns: Vec<ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| {
            // Try to re-create the `StringViewArray` to prevent holding the underlying buffer too long.
            let Some(s) = c.as_string_view_opt() else {
                return Arc::clone(c);
            };
            let ideal_buffer_size: usize = s
                .views()
                .iter()
                .map(|v| {
                    let len = (*v as u32) as usize;
                    if len > 12 {
                        len
                    } else {
                        0
                    }
                })
                .sum();
            let actual_buffer_size = s.get_buffer_memory_size();

            // Re-creating the array copies data and can be time consuming.
            // We only do it if the array is sparse
            if actual_buffer_size > (ideal_buffer_size * 2) {
                // We set the block size to `ideal_buffer_size` so that the new StringViewArray only has one buffer, which accelerate later concat_batches.
                // See https://github.com/apache/arrow-rs/issues/6094 for more details.
                let mut builder = StringViewBuilder::with_capacity(s.len());
                if ideal_buffer_size > 0 {
                    builder = builder.with_block_size(ideal_buffer_size as u32);
                }

                for v in s.iter() {
                    builder.append_option(v);
                }

                let gc_string = builder.finish();

                debug_assert!(gc_string.data_buffers().len() <= 1); // buffer count can be 0 if the `ideal_buffer_size` is 0

                Arc::new(gc_string)
            } else {
                Arc::clone(c)
            }
        })
        .collect();
    RecordBatch::try_new(batch.schema(), new_columns)
        .expect("Failed to re-create the gc'ed record batch")
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;

    use arrow::datatypes::{DataType, Field, Schema};
    use arrow_array::builder::ArrayBuilder;
    use arrow_array::{StringViewArray, UInt32Array};

    #[test]
    fn test_coalesce() {
        let batch = uint32_batch(0..8);
        Test::new()
            .with_batches(std::iter::repeat(batch).take(10))
            // expected output is batches of at least 20 rows (except for the final batch)
            .with_target_batch_size(21)
            .with_expected_output_sizes(vec![24, 24, 24, 8])
            .run()
    }

    #[test]
    fn test_coalesce_with_fetch_larger_than_input_size() {
        let batch = uint32_batch(0..8);
        Test::new()
            .with_batches(std::iter::repeat(batch).take(10))
            // input is 10 batches x 8 rows (80 rows) with fetch limit of 100
            // expected to behave the same as `test_concat_batches`
            .with_target_batch_size(21)
            .with_fetch(Some(100))
            .with_expected_output_sizes(vec![24, 24, 24, 8])
            .run();
    }

    #[test]
    fn test_coalesce_with_fetch_less_than_input_size() {
        let batch = uint32_batch(0..8);
        Test::new()
            .with_batches(std::iter::repeat(batch).take(10))
            // input is 10 batches x 8 rows (80 rows) with fetch limit of 50
            .with_target_batch_size(21)
            .with_fetch(Some(50))
            .with_expected_output_sizes(vec![24, 24, 2])
            .run();
    }

    #[test]
    fn test_coalesce_with_fetch_less_than_target_and_no_remaining_rows() {
        let batch = uint32_batch(0..8);
        Test::new()
            .with_batches(std::iter::repeat(batch).take(10))
            // input is 10 batches x 8 rows (80 rows) with fetch limit of 48
            .with_target_batch_size(21)
            .with_fetch(Some(48))
            .with_expected_output_sizes(vec![24, 24])
            .run();
    }

    #[test]
    fn test_coalesce_with_fetch_less_target_batch_size() {
        let batch = uint32_batch(0..8);
        Test::new()
            .with_batches(std::iter::repeat(batch).take(10))
            // input is 10 batches x 8 rows (80 rows) with fetch limit of 10
            .with_target_batch_size(21)
            .with_fetch(Some(10))
            .with_expected_output_sizes(vec![10])
            .run();
    }

    #[test]
    fn test_coalesce_single_large_batch_over_fetch() {
        let large_batch = uint32_batch(0..100);
        Test::new()
            .with_batch(large_batch)
            .with_target_batch_size(20)
            .with_fetch(Some(7))
            .with_expected_output_sizes(vec![7])
            .run()
    }

    /// Test for [`BatchCoalescer`]
    ///
    /// Pushes the input batches to the coalescer and verifies that the resulting
    /// batches have the expected number of rows and contents.
    #[derive(Debug, Clone, Default)]
    struct Test {
        /// Batches to feed to the coalescer. Tests must have at least one
        /// schema
        input_batches: Vec<RecordBatch>,
        /// Expected output sizes of the resulting batches
        expected_output_sizes: Vec<usize>,
        /// target batch size
        target_batch_size: usize,
        /// Fetch (limit)
        fetch: Option<usize>,
    }

    impl Test {
        fn new() -> Self {
            Self::default()
        }

        /// Set the target batch size
        fn with_target_batch_size(mut self, target_batch_size: usize) -> Self {
            self.target_batch_size = target_batch_size;
            self
        }

        /// Set the fetch (limit)
        fn with_fetch(mut self, fetch: Option<usize>) -> Self {
            self.fetch = fetch;
            self
        }

        /// Extend the input batches with `batch`
        fn with_batch(mut self, batch: RecordBatch) -> Self {
            self.input_batches.push(batch);
            self
        }

        /// Extends the input batches with `batches`
        fn with_batches(
            mut self,
            batches: impl IntoIterator<Item = RecordBatch>,
        ) -> Self {
            self.input_batches.extend(batches);
            self
        }

        /// Extends `sizes` to expected output sizes
        fn with_expected_output_sizes(
            mut self,
            sizes: impl IntoIterator<Item = usize>,
        ) -> Self {
            self.expected_output_sizes.extend(sizes);
            self
        }

        /// Runs the test -- see documentation on [`Test`] for details
        fn run(self) {
            let Self {
                input_batches,
                target_batch_size,
                fetch,
                expected_output_sizes,
            } = self;

            let schema = input_batches[0].schema();

            // create a single large input batch for output comparison
            let single_input_batch = concat_batches(&schema, &input_batches).unwrap();

            let mut coalescer =
                BatchCoalescer::new(Arc::clone(&schema), target_batch_size, fetch);

            let mut output_batches = vec![];
            for batch in input_batches {
                match coalescer.push_batch(batch) {
                    CoalescerState::Continue => {}
                    CoalescerState::LimitReached => {
                        output_batches.push(coalescer.finish_batch().unwrap());
                        break;
                    }
                    CoalescerState::TargetReached => {
                        coalescer.buffered_rows = 0;
                        output_batches.push(coalescer.finish_batch().unwrap());
                    }
                }
            }
            if coalescer.buffered_rows != 0 {
                output_batches.extend(coalescer.buffer);
            }

            // make sure we got the expected number of output batches and content
            let mut starting_idx = 0;
            assert_eq!(expected_output_sizes.len(), output_batches.len());
            for (i, (expected_size, batch)) in
                expected_output_sizes.iter().zip(output_batches).enumerate()
            {
                assert_eq!(
                    *expected_size,
                    batch.num_rows(),
                    "Unexpected number of rows in Batch {i}"
                );

                // compare the contents of the batch (using `==` compares the
                // underlying memory layout too)
                let expected_batch =
                    single_input_batch.slice(starting_idx, *expected_size);
                let batch_strings = batch_to_pretty_strings(&batch);
                let expected_batch_strings = batch_to_pretty_strings(&expected_batch);
                let batch_strings = batch_strings.lines().collect::<Vec<_>>();
                let expected_batch_strings =
                    expected_batch_strings.lines().collect::<Vec<_>>();
                assert_eq!(
                    expected_batch_strings, batch_strings,
                    "Unexpected content in Batch {i}:\
                    \n\nExpected:\n{expected_batch_strings:#?}\n\nActual:\n{batch_strings:#?}"
                );
                starting_idx += *expected_size;
            }
        }
    }

    /// Return a batch of  UInt32 with the specified range
    fn uint32_batch(range: Range<u32>) -> RecordBatch {
        let schema =
            Arc::new(Schema::new(vec![Field::new("c0", DataType::UInt32, false)]));

        RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(UInt32Array::from_iter_values(range))],
        )
        .unwrap()
    }

    #[test]
    fn test_gc_string_view_batch_small_no_compact() {
        // view with only short strings (no buffers) --> no need to compact
        let array = StringViewTest {
            rows: 1000,
            strings: vec![Some("a"), Some("b"), Some("c")],
        }
        .build();

        let gc_array = do_gc(array.clone());
        compare_string_array_values(&array, &gc_array);
        assert_eq!(array.data_buffers().len(), 0);
        assert_eq!(array.data_buffers().len(), gc_array.data_buffers().len()); // no compaction
    }

    #[test]
    fn test_gc_string_view_batch_large_no_compact() {
        // view with large strings (has buffers) but full --> no need to compact
        let array = StringViewTest {
            rows: 1000,
            strings: vec![Some("This string is longer than 12 bytes")],
        }
        .build();

        let gc_array = do_gc(array.clone());
        compare_string_array_values(&array, &gc_array);
        assert_eq!(array.data_buffers().len(), 5);
        assert_eq!(array.data_buffers().len(), gc_array.data_buffers().len()); // no compaction
    }

    #[test]
    fn test_gc_string_view_batch_large_slice_compact() {
        // view with large strings (has buffers) and only partially used  --> no need to compact
        let array = StringViewTest {
            rows: 1000,
            strings: vec![Some("this string is longer than 12 bytes")],
        }
        .build();

        // slice only 11 rows, so most of the buffer is not used
        let array = array.slice(11, 22);

        let gc_array = do_gc(array.clone());
        compare_string_array_values(&array, &gc_array);
        assert_eq!(array.data_buffers().len(), 5);
        assert_eq!(gc_array.data_buffers().len(), 1); // compacted into a single buffer
    }

    /// Compares the values of two string view arrays
    fn compare_string_array_values(arr1: &StringViewArray, arr2: &StringViewArray) {
        assert_eq!(arr1.len(), arr2.len());
        for (s1, s2) in arr1.iter().zip(arr2.iter()) {
            assert_eq!(s1, s2);
        }
    }

    /// runs garbage collection on string view array
    /// and ensures the number of rows are the same
    fn do_gc(array: StringViewArray) -> StringViewArray {
        let batch =
            RecordBatch::try_from_iter(vec![("a", Arc::new(array) as ArrayRef)]).unwrap();
        let gc_batch = gc_string_view_batch(&batch);
        assert_eq!(batch.num_rows(), gc_batch.num_rows());
        assert_eq!(batch.schema(), gc_batch.schema());
        gc_batch
            .column(0)
            .as_any()
            .downcast_ref::<StringViewArray>()
            .unwrap()
            .clone()
    }

    /// Describes parameters for creating a `StringViewArray`
    struct StringViewTest {
        /// The number of rows in the array
        rows: usize,
        /// The strings to use in the array (repeated over and over
        strings: Vec<Option<&'static str>>,
    }

    impl StringViewTest {
        /// Create a `StringViewArray` with the parameters specified in this struct
        fn build(self) -> StringViewArray {
            let mut builder = StringViewBuilder::with_capacity(100).with_block_size(8192);
            loop {
                for &v in self.strings.iter() {
                    builder.append_option(v);
                    if builder.len() >= self.rows {
                        return builder.finish();
                    }
                }
            }
        }
    }
    fn batch_to_pretty_strings(batch: &RecordBatch) -> String {
        arrow::util::pretty::pretty_format_batches(&[batch.clone()])
            .unwrap()
            .to_string()
    }
}
