#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;
use common::Fixture;
use nix::libc;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

fn run(fixture: &Fixture, mode: &str) -> (Value, Vec<Value>) {
    let output = fixture.run(&format!("exec ./fixture {mode}"));
    assert!(
        output.status.success(),
        "{mode}: {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let sample = serde_json::from_slice(&output.stdout).unwrap();
    (sample, fixture.events())
}

fn check_identity_and_result(event: &Value, sample: &Value) {
    assert_eq!(event["source"], "ptrace");
    let data = &event["data"];
    assert_eq!(data["syscall"], "mmap");
    assert_eq!(data["pid"], sample["pid"]);
    assert!(data.get("tid").is_none());
    assert!(data["process_instance_id"].as_u64().unwrap() > 0);
    assert!(data["duration_ns"].as_u64().is_some());
    let started = data["started_unix_ns"].as_u64().unwrap();
    let completed = data["completed_unix_ns"].as_u64().unwrap();
    assert!(started > 0 && completed >= started);
    assert_eq!(data["start_source"], "syscall_entry");
    assert_eq!(data["address"], "0x0");
    assert_eq!(data["restart"], false);
    if let Some(address) = data["mapped_address"].as_str() {
        assert_eq!(
            data["result"].as_u64().unwrap(),
            u64::from_str_radix(address.trim_start_matches("0x"), 16).unwrap()
        );
        assert!(data["errno"].is_null());
    } else {
        assert_eq!(
            data["result"].as_i64().unwrap(),
            -data["errno"].as_i64().unwrap()
        );
    }
}

#[test]
fn anonymous_mappings_preserve_memory_behavior_and_task_ownership() {
    for mode in ["anonymous", "thread", "fork"] {
        let fixture = Fixture::compile("mmap");
        let (sample, events) = run(&fixture, mode);
        let event = events
            .iter()
            .find(|event| {
                event["kind"] == "memory.mmap"
                    && event["data"]["mapped_address"] == sample["address"]
                    && event["data"]["fd"] == 12345
            })
            .expect("the real anonymous mmap was not recorded");
        check_identity_and_result(event, &sample);
        let data = &event["data"];
        assert_eq!(data["length"], sample["length"]);
        assert_eq!(data["anonymous"], true);
        assert_eq!(data["prot"], libc::PROT_READ | libc::PROT_WRITE);
        assert_eq!(data["flags"], libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
        assert_eq!(data["offset"], 0);
        assert!(data.get("resolved_path").is_none());
        assert!(data.get("capture_error").is_none());
        let root = events
            .iter()
            .find(|event| {
                event["kind"] == "process.exec" && event["data"]["argv"][0] == "./fixture"
            })
            .unwrap();
        if mode == "thread" {
            assert_eq!(sample["pid"], root["data"]["pid"]);
            assert_ne!(sample["tid"], sample["pid"]);
        } else if mode == "fork" {
            assert_ne!(sample["pid"], root["data"]["pid"]);
        }
        assert!(events.iter().any(|event| {
            event["kind"] == "process.exit"
                && event["data"]["pid"] == sample["pid"]
                && event["data"]["process_instance_id"] == data["process_instance_id"]
        }));
    }
}

#[test]
fn file_mappings_record_offsets_paths_and_preserve_shared_private_semantics() {
    let fixture = Fixture::compile("mmap");
    let (sample, events) = run(&fixture, "file");
    let path = fixture.0.join(OsStr::from_bytes(b"mapped-\xff.bin"));
    for (address, flags) in [
        ("shared_address", libc::MAP_SHARED),
        ("private_address", libc::MAP_PRIVATE),
    ] {
        let event = events
            .iter()
            .find(|event| {
                event["kind"] == "memory.mmap"
                    && event["data"]["mapped_address"] == sample[address]
                    && event["data"]["flags"] == flags
            })
            .expect("the real file mmap was not recorded");
        check_identity_and_result(event, &sample);
        let data = &event["data"];
        assert_eq!(data["anonymous"], false);
        assert_eq!(data["length"], sample["page"]);
        assert_eq!(data["offset"], sample["page"]);
        assert_eq!(data["fd"], sample["fd"]);
        assert_eq!(data["prot"], libc::PROT_READ | libc::PROT_WRITE);
        assert_eq!(data["path_capture"], "complete");
        assert_eq!(data["resolved_path"], path.to_string_lossy().as_ref());
        assert_eq!(data["path_bytes"], json!(path.as_os_str().as_bytes()));
    }
    let contents = std::fs::read(path).unwrap();
    let page = sample["page"].as_u64().unwrap() as usize;
    assert!(contents[..page].iter().all(|byte| *byte == 0));
    assert_eq!(&contents[page..page + 8], b"Sriginal");
}

#[test]
fn failed_mappings_preserve_errno_and_do_not_claim_a_mapped_address() {
    let fixture = Fixture::compile("mmap");
    let (sample, events) = run(&fixture, "failures");
    for (length, fd, offset, errno) in [
        (0, -1, 0, libc::EINVAL),
        (sample["page"].as_u64().unwrap(), -1, 0, libc::EBADF),
        (
            sample["page"].as_u64().unwrap(),
            sample["fd"].as_i64().unwrap(),
            1,
            libc::EINVAL,
        ),
    ] {
        let event = events
            .iter()
            .find(|event| {
                let data = &event["data"];
                event["kind"] == "memory.mmap"
                    && data["length"] == length
                    && data["fd"] == fd
                    && data["offset"] == offset
                    && data["errno"] == errno
            })
            .expect("the failed mmap was not recorded");
        check_identity_and_result(event, &sample);
        let data = &event["data"];
        assert!(data["mapped_address"].is_null());
        if errno == libc::EBADF {
            assert_eq!(data["path_capture"], "unreadable");
            assert!(data["capture_error"].is_string());
        }
    }
}
