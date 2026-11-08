#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use serde_json::Value;
mod common;
use common::Fixture;

fn fixture() -> Fixture {
    let fixture = Fixture::compile("file_preview");
    std::fs::write(fixture.0.join("example.json"), b"{\"ok\":true}\n\n").unwrap();
    std::fs::write(fixture.0.join("empty.txt"), b"").unwrap();
    std::fs::write(fixture.0.join("binary.bin"), [0, 255, 65, 10]).unwrap();
    fixture
}

fn open<'a>(events: &'a [Value], path: &str) -> &'a Value {
    &events
        .iter()
        .find(|event| event["kind"] == "file.open" && event["data"]["path"] == path)
        .unwrap()["data"]
}

#[test]
fn snapshot_survives_path_replacement_and_preserves_application_read_offset() {
    let fixture = fixture();
    let output = fixture.run("exec ./fixture");
    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"FILE_PREVIEW_OK\n");
    let events = fixture.events();
    let opened = open(&events, "example.json");
    let start = opened["started_unix_ns"]
        .as_u64()
        .expect("syscall start time");
    let end = opened["completed_unix_ns"]
        .as_u64()
        .expect("syscall completion time");
    assert!(end >= start);
    assert_eq!(opened["start_source"], "syscall_entry");
    let preview = &open(&events, "example.json")["preview"];
    assert_eq!(preview["status"], "complete");
    assert_eq!(preview["captured_bytes"], 13);
    let payload = std::fs::read(fixture.0.join("run/file-previews.bin")).unwrap();
    let offset = preview["offset"].as_u64().unwrap() as usize;
    assert_eq!(&payload[offset..offset + 13], b"{\"ok\":true}\n\n");
    assert_eq!(
        std::fs::read(fixture.0.join("example.json")).unwrap(),
        b"replacement"
    );
    let empty = &open(&events, "empty.txt")["preview"];
    assert_eq!(empty["status"], "complete");
    assert_eq!(empty["captured_bytes"], 0);
    let binary = &open(&events, "binary.bin")["preview"];
    let offset = binary["offset"].as_u64().unwrap() as usize;
    assert_eq!(&payload[offset..offset + 4], &[0, 255, 65, 10]);
    for (path, reason) in [
        ("/dev/null", "not_regular"),
        (".", "not_regular"),
        ("/proc/self/stat", "unsupported_filesystem"),
        ("does-not-exist", "open_failed"),
    ] {
        let event = open(&events, path);
        assert_eq!(event["preview"]["status"], "unavailable", "{event}");
        assert_eq!(event["preview"]["reason"], reason, "{event}");
        assert!(event["preview"]["offset"].is_null());
    }
    let write = events
        .iter()
        .find(|event| {
            event["kind"] == "file.open"
                && event["data"]["path"] == "example.json"
                && event["data"]["flags"].as_i64().unwrap() & 3 == 1
        })
        .unwrap();
    assert_eq!(write["data"]["preview"]["reason"], "write_only");
}

#[test]
fn preview_limit_truncates_payload_without_truncating_target_reads() {
    let fixture = fixture();
    let mut command = fixture.command("exec ./fixture");
    command.args(["--file-preview-max-bytes", "4"]);
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"FILE_PREVIEW_OK\n");
    let events = fixture.events();
    let preview = &open(&events, "example.json")["preview"];
    assert_eq!(preview["status"], "truncated");
    assert_eq!(preview["captured_bytes"], 4);
    let payload = std::fs::read(fixture.0.join("run/file-previews.bin")).unwrap();
    let offset = preview["offset"].as_u64().unwrap() as usize;
    assert_eq!(&payload[offset..offset + 4], b"{\"ok");
}

#[test]
fn exhausted_or_disabled_capture_keeps_program_behavior_and_open_events() {
    for (args, reason, payload_size) in [
        (
            ["--file-preview-total-bytes", "1"],
            "budget_exhausted",
            Some(1),
        ),
        (["--file-preview-max-bytes", "0"], "disabled", None),
    ] {
        let fixture = fixture();
        let mut command = fixture.command("exec ./fixture");
        command.args(args);
        let output = Fixture::wait(command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"FILE_PREVIEW_OK\n");
        let events = fixture.events();
        let preview = &open(&events, "example.json")["preview"];
        assert_eq!(preview["status"], "unavailable");
        assert_eq!(preview["reason"], reason);
        assert!(preview["offset"].is_null());
        let payload = std::fs::metadata(fixture.0.join("run/file-previews.bin")).ok();
        assert_eq!(payload.map(|file| file.len()), payload_size);
    }
}
