use std::collections::HashMap;

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use serde_json::{json, Map, Value};
use statsig_forward_proxy::datastore::log_event_store::LogEventStore;
use statsig_forward_proxy::datatypes::log_event::{
    EventName, LogEvent, LogEventRequest, PossiblyLogEvent, StatsigMetadata, User,
};

const SDK_KEY: &str = "secret-server-key";

fn build_store() -> LogEventStore {
    let http_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("benchmark client should build");
    LogEventStore::new("http://127.0.0.1:1", http_client, 10_000_000)
}

fn gate_exposure(seed: usize, time_ms: u64) -> LogEvent {
    let mut metadata = HashMap::new();
    metadata.insert("gate".to_string(), json!(format!("gate_{}", seed % 128)));
    metadata.insert("ruleID".to_string(), json!(format!("rule_{}", seed % 16)));
    metadata.insert("gateValue".to_string(), json!(seed.is_multiple_of(2)));

    let mut custom_ids = Map::new();
    custom_ids.insert(
        "deviceID".to_string(),
        json!(format!("device_{}", seed % 2048)),
    );

    LogEvent {
        event_name: EventName::GateExposure,
        user: Some(User {
            user_id: Some(format!("user_{}", seed % 10_000)),
            custom_ids: Some(Value::Object(custom_ids)),
            extra: HashMap::new(),
        }),
        time: Some(time_ms),
        metadata: Some(metadata),
        statsig_metadata: None,
        extra: HashMap::new(),
    }
}

fn config_exposure(seed: usize, time_ms: u64) -> LogEvent {
    let mut metadata = HashMap::new();
    metadata.insert("config".to_string(), json!(format!("config_{}", seed % 64)));
    metadata.insert("ruleID".to_string(), json!(format!("rule_{}", seed % 16)));

    LogEvent {
        event_name: EventName::ConfigExposure,
        user: Some(User {
            user_id: Some(format!("user_{}", seed % 10_000)),
            custom_ids: None,
            extra: HashMap::new(),
        }),
        time: Some(time_ms),
        metadata: Some(metadata),
        statsig_metadata: None,
        extra: HashMap::new(),
    }
}

fn build_log_event_request(total_events: usize, unique_events: usize) -> LogEventRequest {
    let base_time_ms = 1_730_000_000_000_u64;
    let unique_events = unique_events.max(1).min(total_events.max(1));
    let mut events = Vec::with_capacity(total_events);

    for i in 0..total_events {
        let seed = i % unique_events;
        let rounded_time = base_time_ms + ((seed % 6) as u64) * 1_000;
        let event = if seed.is_multiple_of(2) {
            gate_exposure(seed, rounded_time)
        } else {
            config_exposure(seed, rounded_time)
        };
        events.push(PossiblyLogEvent::ValidLogEvent(Box::new(event)));
    }

    LogEventRequest {
        events,
        statsig_metadata: Some(StatsigMetadata {
            stable_id: Some("stable_id_for_benchmark".to_string()),
            extra: HashMap::new(),
        }),
        extra: HashMap::new(),
    }
}

fn benchmark_log_event_dedup_and_repackage(c: &mut Criterion) {
    let mut group = c.benchmark_group("v1_log_event");
    group.sample_size(30);

    for total_events in [1_000usize, 10_000usize] {
        let unique_events = total_events / 4;
        group.throughput(Throughput::Elements(total_events as u64));

        group.bench_with_input(
            BenchmarkId::new("dedupe_only", total_events),
            &total_events,
            |b, &count| {
                b.iter_batched(
                    || (build_store(), build_log_event_request(count, unique_events)),
                    |(store, mut request)| {
                        let stats = store.dedupe_events_for_sdk_key(SDK_KEY, &mut request);
                        black_box(stats);
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("dedupe_and_repackage", total_events),
            &total_events,
            |b, &count| {
                b.iter_batched(
                    || (build_store(), build_log_event_request(count, unique_events)),
                    |(store, mut request)| {
                        let repackaged = store
                            .dedupe_and_serialize_for_sdk_key(SDK_KEY, &mut request)
                            .expect("benchmark request should serialize");
                        black_box(repackaged.stats);
                        black_box(repackaged.body.len());
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(benches, benchmark_log_event_dedup_and_repackage);
criterion_main!(benches);
