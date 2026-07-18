use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
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

fn parse_stdout<T: DeserializeOwned>(output: &Output) -> T {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout was not one JSON envelope: {err}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[derive(Deserialize)]
struct EnvelopeHeader {
    schema: String,
    state: String,
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
        let body: EnvelopeHeader = parse_stdout(&output);
        assert_eq!(body.schema, "scout.cli.response.v1", "args={args:?}");
        assert_eq!(body.state, state, "args={args:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    }
}

#[derive(Serialize)]
struct TestEvalSuite<'a> {
    queries: &'a [TestEvalQuery],
}

#[derive(Serialize)]
struct TestEvalQuery {
    id: &'static str,
    corpus_root: PathBuf,
    query: &'static str,
    must_find: Vec<TestGoldenFact>,
    poison: Vec<TestGoldenPoison>,
    negative: bool,
}

#[derive(Serialize)]
struct TestGoldenFact {
    fact: String,
    file: String,
    line: usize,
}

#[derive(Serialize)]
struct TestGoldenPoison {
    fact: String,
}

#[derive(Deserialize)]
struct EvalEnvelope {
    state: String,
    data: EvalData,
}

#[derive(Deserialize)]
struct EvalData {
    results: Vec<TestEvalResult>,
    metrics: TestEvalMetrics,
    gate_results: TestEvalGateResults,
    summary_path: String,
}

#[derive(Deserialize)]
struct TestEvalResult {
    #[serde(rename = "id")]
    _id: String,
}

#[derive(Deserialize)]
struct TestEvalMetrics {
    candidate_file_recall: f64,
    coverage: f64,
}

#[derive(Deserialize)]
struct TestEvalGateResults {
    suite_complete: bool,
}

fn write_eval_suite(root: &Path, queries: &[TestEvalQuery]) {
    let goldens = root.join("eval/goldens");
    fs::create_dir_all(&goldens).unwrap();
    fs::write(
        goldens.join("code.json"),
        serde_json::to_vec(&TestEvalSuite { queries }).unwrap(),
    )
    .unwrap();
}

#[test]
fn eval_zero_cap_stops_before_work_and_writes_a_partial_summary() {
    let dir = tempfile::tempdir().unwrap();
    write_eval_suite(
        dir.path(),
        &[TestEvalQuery {
            id: "q1",
            corpus_root: dir.path().join("corpus"),
            query: "anything",
            must_find: Vec::new(),
            poison: Vec::new(),
            negative: true,
        }],
    );

    let output = scout(&["eval", "m1", "--yes", "--max-dollars", "0"], dir.path());
    assert_eq!(output.status.code(), Some(3));
    let body: EvalEnvelope = parse_stdout(&output);
    assert_eq!(body.state, "partial");
    assert!(body.data.results.is_empty());
    assert_eq!(body.data.metrics.candidate_file_recall, 100.0);
    assert_eq!(body.data.metrics.coverage, 100.0);
    assert!(!body.data.gate_results.suite_complete);
    assert!(dir.path().join(body.data.summary_path).is_file());
}
