//! Opt-in, synthetic conversion benchmark. No sockets, models or credentials.
use emp_router::{ProjectionIds, response_json_stream_events};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn allocated(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
// SAFETY: All allocation operations delegate to System using the same layout.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let updated = unsafe { System.realloc(pointer, layout, size) };
        if !updated.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            allocated(size);
        }
        updated
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1".into())
        .parse()
        .unwrap();
    assert!((1..=16).contains(&workers));
    for case in [
        "long_answer",
        "many_parts",
        "tool_history",
        "repeated_metadata",
    ] {
        let live = LIVE.load(Ordering::Relaxed);
        PEAK.store(live, Ordering::Relaxed);
        let started = Instant::now();
        let barrier = Arc::new(Barrier::new(workers));
        let results = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..workers).map(|_| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let output = match case {
                        "long_answer" => json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer 世界\n".repeat(65536)}]}]),
                        "many_parts" => json!([{"type":"message","role":"assistant","id":"id".repeat(1024),"content":vec![json!({"type":"output_text","text":"part text\n".repeat(2048)});32]}]),
                        "repeated_metadata" => json!([{"type":"message","role":"assistant","id":"x".repeat(64 * 1024),"content":vec![json!({"type":"output_text","text":"ok"});256]}]),
                        _ => json!((0..64).map(|n| json!({"type":"function_call","call_id":format!("call_{n}"),"name":"read","arguments":json!({"path":format!("src/{n}.rs"),"context":"code\n".repeat(1024)}).to_string()})).collect::<Vec<_>>()),
                    };
                    let response = json!({"status":"completed","output":output,"model":"fixture","usage":{"input_tokens":40000,"output_tokens":4000}});
                    let ids = ProjectionIds::new("resp_fixture", "msg_fixture", "r", "lr");
                    barrier.wait();
                    let events = response_json_stream_events(response, &ids, true).unwrap();
                    let mut bytes = 0;
                    let mut count = 0;
                    let mut terminal = false;
                    for event in events {
                        bytes += serde_json::to_vec(&event).unwrap().len();
                        count += 1;
                        terminal = event["type"] == "response.completed";
                    }
                    assert!(terminal);
                    (bytes, count)
                })
            }).collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        let elapsed_us = started.elapsed().as_micros();
        let peak_bytes = PEAK.load(Ordering::Relaxed).saturating_sub(live);
        assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
        println!(
            "{}",
            json!({"scenario":case,"workers":workers,"elapsed_us":elapsed_us,"peak_heap_bytes":peak_bytes,"output_bytes_per_request":results[0].0,"events_per_request":results[0].1})
        );
    }
}
