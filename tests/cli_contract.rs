use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn scout(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scout"))
        .args(args)
        .current_dir(cwd)
        .env_remove("CEREBRAS_API_KEY")
        .env_remove("SCOUT_API_KEY")
        .output()
        .unwrap()
}

fn envelope(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout was not one JSON envelope: {err}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn binary_error_paths_emit_one_envelope_with_matching_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    for (args, code, state) in [
        (vec![], 2, "usage_error"),
        (vec!["--not-a-real-flag"], 2, "usage_error"),
        (vec!["--query", "anything", "."], 12, "index_missing"),
        (vec!["doctor"], 13, "provider_error"),
    ] {
        let output = scout(&args, dir.path());
        assert_eq!(output.status.code(), Some(code), "args={args:?}");
        let body = envelope(&output);
        assert_eq!(body["schema"], "scout.cli.response.v1", "args={args:?}");
        assert_eq!(body["state"], state, "args={args:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    }
}

fn write_eval_suite(root: &Path, queries: Value) {
    let goldens = root.join("eval/goldens");
    fs::create_dir_all(&goldens).unwrap();
    fs::write(
        goldens.join("code.json"),
        serde_json::to_vec(&json!({"queries": queries})).unwrap(),
    )
    .unwrap();
}

#[test]
fn eval_zero_cap_stops_before_work_and_writes_a_partial_summary() {
    let dir = tempfile::tempdir().unwrap();
    write_eval_suite(
        dir.path(),
        json!([{
            "id": "q1",
            "corpus_root": dir.path().join("corpus"),
            "query": "anything",
            "must_find": [],
            "poison": [],
            "negative": true
        }]),
    );

    let output = scout(&["eval", "m1", "--yes", "--max-dollars", "0"], dir.path());
    assert_eq!(output.status.code(), Some(3));
    let body = envelope(&output);
    assert_eq!(body["state"], "partial");
    assert_eq!(body["data"]["results"], json!([]));
    assert_eq!(body["data"]["metrics"]["candidate_file_recall"], 100.0);
    assert_eq!(body["data"]["metrics"]["coverage"], 100.0);
    assert_eq!(body["data"]["gate_results"]["suite_complete"], false);
    let summary = body["data"]["summary_path"].as_str().unwrap();
    assert!(dir.path().join(summary).is_file());
}
