use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysinfo::{Disks, System};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

// Pull in the generated types and the trait we need to implement.
// The string "metrics" matches the `package metrics;` in your .proto file.
use crate::proto::metrics_service_server::MetricsService;
use crate::proto::{MetricSnapshot, StreamRequest};

// ─── SERVICE STRUCT ───────────────────────────────────────────────────────────
//
// This is your service handle. It's cheaply cloneable because tonic may
// instantiate it per connection. Add shared state here with Arc<T> when needed
// (e.g. a database pool, a config store).

#[derive(Debug, Default)]
pub struct MetricsServiceImpl;

// ─── TRAIT IMPLEMENTATION ─────────────────────────────────────────────────────
//
// This trait was generated from your .proto file.
// If your proto has 3 RPCs, this trait has 3 methods you must implement.
// The compiler will tell you exactly what's missing.

#[tonic::async_trait]
impl MetricsService for MetricsServiceImpl {
    // The return type is generated from `returns (stream MetricSnapshot)`.
    // ReceiverStream<T> is a tokio mpsc receiver that implements Stream —
    // exactly the bridge tonic needs to push messages back to the client.
    type StreamMetricsStream = ReceiverStream<Result<MetricSnapshot, Status>>;

    async fn stream_metrics(
        &self,
        request: Request<StreamRequest>,
    ) -> Result<Response<Self::StreamMetricsStream>, Status> {
        // Clamp the interval: respect the client's wish but never go below
        // 100ms — a tight loop hammering sysinfo would spike your CPU.
        let interval_ms = request.into_inner().interval_ms.clamp(100, 10_000) as u64;

        // ── CHANNEL ──────────────────────────────────────────────────────────
        //
        // tx (sender) lives in the spawned task and pushes snapshots.
        // rx (receiver) is wrapped in ReceiverStream and returned to tonic,
        // which forwards each item to the connected client.
        //
        // Buffer of 16: if the client is slow to consume, we'll block the
        // producer rather than accumulate unbounded memory.
        let (tx, rx) = mpsc::channel(16);

        // ── PRODUCER TASK ─────────────────────────────────────────────────────
        //
        // This runs independently on the tokio thread pool.
        // It ends naturally when tx.send() fails — which happens the moment
        // the client disconnects and the receiver is dropped.
        // No explicit cancellation needed.
        tokio::spawn(async move {
            let mut sys = System::new_all();
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));

            loop {
                interval.tick().await;

                let snapshot = collect_snapshot(&mut sys);

                // If the client has disconnected, send() returns Err.
                // We break cleanly — no panic, no resource leak.
                if tx.send(Ok(snapshot)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ─── SNAPSHOT COLLECTION ──────────────────────────────────────────────────────
//
// Separated into its own function for two reasons:
//   1. It keeps the async fn above focused on gRPC concerns.
//   2. It's easy to unit test independently.

fn collect_snapshot(sys: &mut System) -> MetricSnapshot {
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let cpu = sys.global_cpu_info().cpu_usage();

    let memory = if sys.total_memory() > 0 {
        (sys.used_memory() as f32 / sys.total_memory() as f32) * 100.0
    } else {
        0.0
    };

    let disk = collect_disk_usage();

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    MetricSnapshot {
        cpu_usage_percent: cpu,
        memory_usage_percent: memory,
        disk_usage_percent: disk,
        timestamp_unix_ms: timestamp,
    }
}

fn collect_disk_usage() -> f32 {
    let disks = Disks::new_with_refreshed_list();

    let (total, used) = disks.iter().fold((0u64, 0u64), |(t, u), disk| {
        (
            t + disk.total_space(),
            u + (disk.total_space().saturating_sub(disk.available_space())),
        )
    });

    if total > 0 {
        (used as f32 / total as f32) * 100.0
    } else {
        0.0
    }
}
