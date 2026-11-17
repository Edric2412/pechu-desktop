//! Tests with fake `pw-record` / `pw-dump` scripts, so nothing touches the real sound system.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Writes an executable shell script into `dir`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake pw-record: notes its own process id and the environment it got, prints `data`, then waits until it is killed.
/// (`exec` keeps the same process id, so the id in the file is the id of the process that has to die.)
fn fake_record(dir: &Path, data: &[u8], then: &str) -> PathBuf {
    std::fs::write(dir.join("pcm.bin"), data).unwrap();
    let body = format!(
        "[ \"$1\" = \"--version\" ] && exit 0\necho $$ > '{d}/pid'\nenv > '{d}/env'\necho \"$@\" > '{d}/args'\ncat '{d}/pcm.bin'\n{then}",
        d = dir.display()
    );
    script(dir, "pw-record", &body)
}

fn backend(record: PathBuf, dump: PathBuf) -> Pipewire {
    Pipewire { record_program: record.into(), dump_program: dump.into(), tool_timeout: Duration::from_millis(1500) }
}

/// Two whole chunks of 20 ms, each byte different from its neighbours, so a shifted or mixed-up chunk would show.
fn pattern(chunks: usize) -> Vec<u8> {
    (0..chunks * 3840).map(|i| (i % 251) as u8).collect()
}

fn process_is_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')), // a zombie is dead too
    }
}

#[tokio::test]
async fn chunks_arrive_whole_and_in_order_and_the_child_dies_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(2);
    let record = fake_record(dir.path(), &data, "exec sleep 600");
    let mut stream = backend(record, "unused".into()).start().expect("starts");

    assert_eq!(stream.format, CaptureFormat { sample_rate: 48_000, channels: 2 });
    assert_eq!(stream.next_chunk().await.unwrap(), data[..3840]);
    assert_eq!(stream.next_chunk().await.unwrap(), data[3840..]);

    let pid: u32 = std::fs::read_to_string(dir.path().join("pid")).unwrap().trim().parse().unwrap();
    assert!(!process_is_gone(pid), "the recorder is running while the stream is open");
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !process_is_gone(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(process_is_gone(pid), "dropping the stream must kill the recorder");
}

#[tokio::test]
async fn the_recorder_gets_the_right_arguments_and_almost_no_environment() {
    let dir = tempfile::tempdir().unwrap();
    let record = fake_record(dir.path(), &pattern(1), "exit 0");
    let mut stream = backend(record, "unused".into()).start().unwrap();
    while stream.next_chunk().await.is_some() {}

    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    for wanted in ["--raw", "--rate 48000", "--channels 2", "--format s16", "stream.capture.sink=true"] {
        assert!(args.contains(wanted), "missing {wanted} in: {args}");
    }
    assert!(args.trim_end().ends_with(" -"), "raw sound goes to standard output: {args}");
    assert!(!args.contains("--target"), "no target: it must follow the default output");

    // cargo gives test programs many CARGO_* variables; none of them (nor anything else of ours) may reach the recorder
    let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
    let names: Vec<&str> = env.lines().filter_map(|l| l.split('=').next()).collect();
    let allowed = ["PATH", "HOME", "XDG_RUNTIME_DIR", "PIPEWIRE_REMOTE", "PIPEWIRE_RUNTIME_DIR", "PWD", "SHLVL", "_", "OLDPWD"];
    assert!(names.iter().all(|n| allowed.contains(n)), "unexpected variables reached the recorder: {names:?}");
}

#[tokio::test]
async fn the_stream_ends_when_the_recorder_stops_and_half_a_chunk_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let mut data = pattern(2);
    data.extend_from_slice(&[7u8; 1000]); // a leftover that is not a whole chunk
    let record = fake_record(dir.path(), &data, "exit 0");
    let mut stream = backend(record, "unused".into()).start().unwrap();
    assert!(stream.next_chunk().await.is_some());
    assert!(stream.next_chunk().await.is_some());
    assert!(stream.next_chunk().await.is_none());
}

#[tokio::test]
async fn a_missing_program_is_a_plain_error() {
    let err = backend("/nonexistent/pw-record".into(), "unused".into()).start().err().expect("fails");
    assert!(err.contains("Could not start"), "{err}");
}

#[test]
fn available_needs_both_tools_and_a_sound_output() {
    let dir = tempfile::tempdir().unwrap();
    let record = fake_record(dir.path(), &[], "exit 0");
    let dump = |name: &str, json: &str| {
        std::fs::write(dir.path().join(format!("{name}.json")), json).unwrap();
        script(dir.path(), name, &format!("cat '{}/{name}.json'", dir.path().display()))
    };
    let with_sink = dump("dump-ok", r#"[{"id":1,"info":{"props":{"media.class":"Audio/Source"}}},{"id":2,"info":{"props":{"media.class":"Audio/Sink"}}}]"#);
    let without_sink = dump("dump-none", r#"[{"id":1,"info":{"props":{"media.class":"Audio/Source"}}},{"id":3}]"#);
    let garbage = dump("dump-bad", "not json at all");

    assert_eq!(backend(record.clone(), with_sink.clone()).available(), Ok(()));
    assert!(backend(record.clone(), without_sink).available().unwrap_err().contains("No sound output"));
    assert!(backend(record.clone(), garbage).available().unwrap_err().contains("No sound output"));
    assert!(backend(record, "/nonexistent/pw-dump".into()).available().unwrap_err().contains("not found"));
    assert!(backend("/nonexistent/pw-record".into(), with_sink).available().unwrap_err().contains("not found"));
}

#[test]
fn a_hanging_sound_server_does_not_freeze_the_question() {
    let dir = tempfile::tempdir().unwrap();
    let record = fake_record(dir.path(), &[], "exit 0");
    let hang = script(dir.path(), "pw-dump", "exec sleep 600");
    let mut pw = backend(record, hang);
    pw.tool_timeout = Duration::from_millis(300);
    let started = Instant::now();
    assert!(pw.available().unwrap_err().contains("did not answer"));
    assert!(started.elapsed() < Duration::from_secs(3));
}
