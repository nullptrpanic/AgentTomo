#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;
use common::Fixture;
use serde_json::Value;

#[test]
fn worker_thread_events_use_the_process_identity_and_one_process_exit() {
    let fixture = Fixture::compile("mmap");
    let output = fixture.run("exec ./fixture thread");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sample: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(
        sample["pid"], sample["tid"],
        "exercise a real nonleader thread"
    );
    let events = fixture.events();
    let root = events
        .iter()
        .find(|e| e["kind"] == "process.exec" && e["data"]["argv"][0] == "./fixture")
        .unwrap();
    let instance = root["data"]["process_instance_id"]
        .as_u64()
        .expect("process instance identity");
    let mapped = events
        .iter()
        .find(|e| e["kind"] == "memory.mmap" && e["data"]["mapped_address"] == sample["address"])
        .unwrap();
    assert_eq!(mapped["data"]["pid"], sample["pid"]);
    assert_eq!(mapped["data"]["process_instance_id"], instance);
    assert!(events.iter().all(|e| e["schema_version"] == 2));
    for event in &events {
        for field in [
            "tid",
            "task_id",
            "parent_tid",
            "child_tid",
            "child_task_id",
            "former_tid",
        ] {
            assert!(
                event["data"].get(field).is_none(),
                "thread identity leaked: {event}"
            );
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "process.exit" && e["data"]["pid"] == sample["pid"])
            .count(),
        1
    );
    assert!(
        events.iter().all(|e| e["kind"] != "process.clone"),
        "thread creation must not be a process event"
    );
}
