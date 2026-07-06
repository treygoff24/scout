use anyhow::{Context, Result, anyhow, bail};
use ignore::WalkBuilder;
use rand::Rng;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "scout.cli.response.v1";
const CARD_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MODEL: &str = "gemma-4-31b";
const CEREBRAS_URL: &str = "https://api.cerebras.ai/v1/chat/completions";
const USER_AGENT: &str = concat!(
    "scout/",
    env!("CARGO_PKG_VERSION"),
    " (github.com/treygoff24/scout)"
);
const MAX_FILE_BYTES: u64 = 300_000;
const DEFAULT_CANDIDATES: usize = 20;
const DEFAULT_CONCURRENCY: usize = 50;
const INPUT_DOLLARS_PER_MTOK: f64 = 2.15;
const OUTPUT_DOLLARS_PER_MTOK: f64 = 2.70;
const CARD_PROMPT: &str = "scout-card-v1";
const REFRESH_AUTO_YES_MAX_USD: f64 = 0.25;
const KNOWN_COMMANDS: &[&str] = &[
    "index",
    "brief",
    "capabilities",
    "schema",
    "doctor",
    "eval",
    "help",
];
const HARNESS_DIRS: &[&str] = &[
    ".delegate",
    ".claude",
    ".codex",
    ".cursor",
    ".vscode",
    ".idea",
    ".desloppify",
    ".tldr",
];
const STATE_EXIT_CODES: &[(&str, i32)] = &[
    ("ok", 0),
    ("partial", 3),
    ("unanswered", 4),
    ("budget_hit", 10),
    ("index_stale", 11),
    ("index_missing", 12),
    ("provider_error", 13),
    ("tool_degraded", 14),
    ("usage_error", 2),
    ("internal_error", 1),
];

#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

#[derive(Debug)]
struct ProviderError(String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug)]
struct BudgetHitError(String);

impl std::fmt::Display for BudgetHitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BudgetHitError {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Spend {
    in_tok: u64,
    out_tok: u64,
    usd: f64,
    calls: u64,
    retries: u64,
}

impl Spend {
    fn add(&mut self, usage: Usage, retries: u64) {
        self.in_tok += usage.prompt_tokens;
        self.out_tok += usage.completion_tokens;
        self.calls += 1;
        self.retries += retries;
        self.usd += usage.prompt_tokens as f64 / 1_000_000.0 * INPUT_DOLLARS_PER_MTOK
            + usage.completion_tokens as f64 / 1_000_000.0 * OUTPUT_DOLLARS_PER_MTOK;
    }

    fn merge(&mut self, other: &Spend) {
        self.in_tok += other.in_tok;
        self.out_tok += other.out_tok;
        self.usd += other.usd;
        self.calls += other.calls;
        self.retries += other.retries;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    schema: String,
    state: String,
    command: String,
    root: Option<String>,
    data: Value,
    spend: Spend,
    timings_ms: BTreeMap<String, u128>,
    skipped: Vec<Skip>,
    errors: Vec<String>,
}

impl Envelope {
    fn new(command: &str, state: &str) -> Self {
        Self {
            schema: SCHEMA.to_string(),
            state: state.to_string(),
            command: command.to_string(),
            root: None,
            data: json!({}),
            spend: Spend::default(),
            timings_ms: BTreeMap::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
        }
    }
}

fn usage_error(message: impl Into<String>) -> anyhow::Error {
    anyhow!(UsageError(message.into()))
}

fn provider_error(message: impl Into<String>) -> anyhow::Error {
    anyhow!(ProviderError(message.into()))
}

fn budget_hit_error(message: impl Into<String>) -> anyhow::Error {
    anyhow!(BudgetHitError(message.into()))
}

fn exit_code_for_state(state: &str) -> i32 {
    STATE_EXIT_CODES
        .iter()
        .find_map(|(s, code)| (*s == state).then_some(*code))
        .unwrap_or(1)
}

fn exit_codes_json() -> Value {
    json!(
        STATE_EXIT_CODES
            .iter()
            .copied()
            .collect::<BTreeMap<&str, i32>>()
    )
}

fn state_names() -> Vec<&'static str> {
    STATE_EXIT_CODES.iter().map(|(state, _)| *state).collect()
}

fn finish_env(env: &Envelope, pretty: bool) -> Result<i32> {
    emit(env, pretty)?;
    Ok(exit_code_for_state(&env.state))
}

fn with_exit_code(env: Envelope) -> (Envelope, i32) {
    let code = exit_code_for_state(&env.state);
    (env, code)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Skip {
    path: String,
    reason: String,
    adapter: Option<String>,
}

#[derive(Debug, Clone)]
struct WalkedFile {
    rel: String,
    text: String,
    adapter: String,
    hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Symbol {
    name: String,
    kind: String,
    line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OutlineItem {
    text: String,
    line: usize,
    level: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelHint<T> {
    model_hint: bool,
    value: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Card {
    schema_version: u32,
    path: String,
    hash: String,
    adapter: String,
    symbols: Vec<Symbol>,
    imports: Vec<String>,
    outline: Vec<OutlineItem>,
    churn: u32,
    loc: usize,
    harness_meta: bool,
    role: ModelHint<String>,
    invariants: ModelHint<Vec<String>>,
    gotchas: ModelHint<Vec<String>>,
    terms: ModelHint<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    card_prompt_hash: String,
    model: String,
    adapter_version: String,
    ignore_config_hash: String,
    generated_at_unix_ms: u128,
    root: String,
    cards: usize,
    #[serde(default)]
    markdown_only: bool,
    #[serde(default)]
    file_meta: BTreeMap<String, ManifestFileMeta>,
    skipped: Vec<Skip>,
    unsupported: Vec<Skip>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManifestFileMeta {
    size: u64,
    mtime_unix_ms: u128,
}

#[derive(Debug, Clone)]
struct Snapshot {
    gen_dir: PathBuf,
    manifest: Manifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

impl Message {
    fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Clone)]
struct CerebrasClient {
    api_key: String,
    model: String,
    http: reqwest::blocking::Client,
    spend: Arc<Mutex<Spend>>,
    rate_limiter: Arc<RateLimiter>,
}

impl CerebrasClient {
    fn from_env(spend: Arc<Mutex<Spend>>) -> Result<Self> {
        let api_key = env::var("CEREBRAS_API_KEY")
            .or_else(|_| env::var("SCOUT_API_KEY"))
            .map_err(|_| {
                provider_error(
                    "CEREBRAS_API_KEY is missing; scout will not fake or mock provider results",
                )
            })?;
        let model = env::var("SCOUT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            api_key,
            model,
            http,
            spend,
            rate_limiter: Arc::new(RateLimiter::per_minute(450.0)),
        })
    }

    fn chat_with_budget(
        &self,
        messages: Vec<Message>,
        max_tokens: u64,
        gate: Option<&BudgetGate>,
    ) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
            "temperature": 0,
            "max_completion_tokens": max_tokens,
        });
        let mut retries = 0;
        for attempt in 0..6 {
            self.rate_limiter.wait();
            let response = self
                .http
                .post(CEREBRAS_URL)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send();
            match response {
                Ok(resp) if resp.status().is_success() => {
                    let value: Value = resp.json().context("failed to parse Cerebras JSON")?;
                    let usage: Usage =
                        serde_json::from_value(value.get("usage").cloned().unwrap_or_default())
                            .unwrap_or_default();
                    self.spend.lock().unwrap().add(usage, retries);
                    return value
                        .pointer("/choices/0/message/content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            anyhow!("Cerebras response missing choices[0].message.content")
                        });
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().unwrap_or_default();
                    if !retryable(status) || attempt == 5 {
                        bail!(
                            "Cerebras HTTP {status}: {}",
                            body.chars().take(500).collect::<String>()
                        );
                    }
                    retries += 1;
                    if let Some(gate) = gate {
                        gate.check_retry_budget(&self.spend)?;
                    }
                    sleep_retry(attempt);
                }
                Err(err) => {
                    if attempt == 5 {
                        return Err(err).context("Cerebras request failed after retries");
                    }
                    retries += 1;
                    if let Some(gate) = gate {
                        gate.check_retry_budget(&self.spend)?;
                    }
                    sleep_retry(attempt);
                }
            }
        }
        unreachable!()
    }
}

fn retryable(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

fn sleep_retry(attempt: usize) {
    std::thread::sleep(retry_sleep_duration(attempt));
}

fn retry_sleep_duration(attempt: usize) -> Duration {
    let max_ms = retry_backoff_max_ms(attempt);
    Duration::from_millis(rand::thread_rng().gen_range(0..max_ms))
}

fn retry_backoff_max_ms(attempt: usize) -> u64 {
    if attempt >= 6 {
        60_000
    } else {
        (1_u64 << attempt).min(60) * 1000
    }
}

struct RateLimiter {
    state: Mutex<TokenBucketState>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RateLimiter {
    fn per_minute(requests: f64) -> Self {
        Self {
            state: Mutex::new(TokenBucketState {
                tokens: requests,
                last: Instant::now(),
            }),
            capacity: requests,
            refill_per_sec: requests / 60.0,
        }
    }

    fn wait(&self) {
        loop {
            let wait = {
                let mut state = self.state.lock().unwrap();
                state.take_or_wait(Instant::now(), self.capacity, self.refill_per_sec)
            };
            match wait {
                None => return,
                Some(duration) => std::thread::sleep(duration),
            }
        }
    }
}

#[derive(Debug)]
struct TokenBucketState {
    tokens: f64,
    last: Instant,
}

impl TokenBucketState {
    fn take_or_wait(
        &mut self,
        now: Instant,
        capacity: f64,
        refill_per_sec: f64,
    ) -> Option<Duration> {
        if now > self.last {
            let gained = now.duration_since(self.last).as_secs_f64() * refill_per_sec;
            self.tokens = (self.tokens + gained).min(capacity);
            self.last = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            Some(Duration::from_secs_f64(
                (1.0 - self.tokens) / refill_per_sec,
            ))
        }
    }
}

#[derive(Clone)]
struct BudgetGate {
    max_dollars: Option<f64>,
    hit: Arc<Mutex<Option<String>>>,
}

impl BudgetGate {
    fn new(max_dollars: Option<f64>) -> Self {
        Self {
            max_dollars,
            hit: Arc::new(Mutex::new(None)),
        }
    }

    fn may_launch(&self, spend: &Arc<Mutex<Spend>>, projected: f64) -> bool {
        let mut hit = self.hit.lock().unwrap();
        if hit.is_some() {
            return false;
        }
        if let Some(cap) = self.max_dollars
            && spend.lock().unwrap().usd + projected > cap
        {
            *hit = Some("dollars".to_string());
            return false;
        }
        true
    }

    fn hit(&self) -> Option<String> {
        self.hit.lock().unwrap().clone()
    }

    fn check_retry_budget(&self, spend: &Arc<Mutex<Spend>>) -> Result<()> {
        let mut hit = self.hit.lock().unwrap();
        if hit.is_some() {
            return Err(budget_hit_error("budget cap hit before provider retry"));
        }
        if let Some(cap) = self.max_dollars
            && spend.lock().unwrap().usd >= cap
        {
            *hit = Some("dollars".to_string());
            return Err(budget_hit_error("budget cap hit before provider retry"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Candidate {
    path: String,
    router_rank: Option<usize>,
    deterministic_score: f64,
    look_for: Option<String>,
}

#[derive(Debug, Clone)]
struct Chunk {
    file: String,
    first_line: usize,
    body: String,
    router_rank: Option<usize>,
    deterministic_score: f64,
    look_for: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Finding {
    file: String,
    line: usize,
    fact: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote: Option<String>,
    #[serde(default)]
    quote_omitted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    router_rank: Option<usize>,
    #[serde(default)]
    deterministic_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DroppedFinding {
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote: Option<String>,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    fact: Option<String>,
    line: Option<usize>,
    quote: Option<String>,
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(err) => {
            let state = if err.downcast_ref::<UsageError>().is_some() {
                "usage_error"
            } else if err.downcast_ref::<BudgetHitError>().is_some() {
                "budget_hit"
            } else if err.downcast_ref::<ProviderError>().is_some() {
                "provider_error"
            } else {
                "internal_error"
            };
            let mut env = Envelope::new(&command_name_from_args(), state);
            env.errors.push(format!("{err:#}"));
            finish_env(&env, false).unwrap_or_else(|_| exit_code_for_state(state))
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        let mut env = Envelope::new("unknown", "usage_error");
        env.errors.push("missing command or query".into());
        return finish_env(&env, false);
    }
    match args[0].as_str() {
        "--version" | "-V" => {
            println!("scout {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "--query" => {
            if args.len() < 2 {
                return Err(usage_error("--query needs a query string"));
            }
            cmd_query(&args[1..])
        }
        "index" => cmd_index(&args[1..]),
        "brief" => cmd_brief(&args[1..]),
        "capabilities" => cmd_capabilities(&args[1..]),
        "schema" => cmd_schema(&args[1..]),
        "doctor" => cmd_doctor(&args[1..]),
        "eval" => cmd_eval(&args[1..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(0)
        }
        s if s.starts_with('-') => Err(usage_error(format!("unknown flag {s}"))),
        s if typo_command_hint(s).is_some() => Err(usage_error(format!(
            "unknown command or single-token query {s}; did you mean \"{}\"?",
            typo_command_hint(s).unwrap()
        ))),
        _ => cmd_query(&args),
    }
}

fn command_name_from_args() -> String {
    match env::args().nth(1).as_deref() {
        Some("index" | "brief" | "capabilities" | "schema" | "doctor" | "eval") => {
            env::args().nth(1).unwrap()
        }
        Some("--query") => "query".into(),
        Some(s) if s.starts_with('-') => "unknown".into(),
        Some(_) => "query".into(),
        None => "unknown".into(),
    }
}

fn print_usage() {
    eprintln!("scout index [dir] [--yes] [--max-dollars N] [--pretty]");
    eprintln!(
        "scout [--query] \"query\" [dir] [--budget N] [--max-dollars N] [--refresh] [--compact] [--pretty]"
    );
    eprintln!("scout brief [dir] [--budget N] [--max-dollars N] [--refresh] [--pretty]");
    eprintln!(
        "scout capabilities | schema | doctor | eval m1|m3 [--max-dollars N] [--yes] [--corpus DIR]"
    );
    eprintln!("scout --version | --help");
}

fn arg_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| usage_error(format!("{flag} needs a value")))
}

fn typo_command_hint(token: &str) -> Option<&'static str> {
    if token.chars().any(char::is_whitespace) {
        return None;
    }
    KNOWN_COMMANDS
        .iter()
        .copied()
        .find(|cmd| edit_distance_leq_one(token, cmd))
}

fn edit_distance_leq_one(a: &str, b: &str) -> bool {
    if a == b || a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut i = 0;
    let mut j = 0;
    let mut edits = 0;
    while i < ac.len() && j < bc.len() {
        if ac[i] == bc[j] {
            i += 1;
            j += 1;
        } else {
            edits += 1;
            if edits > 1 {
                return false;
            }
            match ac.len().cmp(&bc.len()) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                Ordering::Greater => i += 1,
                Ordering::Less => j += 1,
            }
        }
    }
    edits + usize::from(i < ac.len() || j < bc.len()) == 1
}

fn parse_money_arg(value: &str, flag: &str) -> Result<f64> {
    value
        .parse()
        .map_err(|err| usage_error(format!("invalid {flag}: {err}")))
}

fn parse_pretty_only(args: &[String], command: &str) -> Result<bool> {
    let mut pretty = false;
    for arg in args {
        if arg == "--pretty" {
            pretty = true;
        } else {
            return Err(usage_error(format!("unknown {command} flag {arg}")));
        }
    }
    Ok(pretty)
}

fn cmd_index(args: &[String]) -> Result<i32> {
    let mut dir = ".".to_string();
    let mut yes = false;
    let mut pretty = false;
    let mut max_dollars = None;
    let mut allow_sensitive = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--yes" | "-y" => yes = true,
            "--pretty" => pretty = true,
            "--allow-sensitive" => allow_sensitive = true,
            "--max-dollars" => {
                i += 1;
                max_dollars = Some(parse_money_arg(
                    arg_value(args, i, "--max-dollars")?,
                    "--max-dollars",
                )?);
            }
            s if !s.starts_with('-') => dir = s.to_string(),
            other => return Err(usage_error(format!("unknown index flag {other}"))),
        }
        i += 1;
    }
    if allow_sensitive {
        return Err(usage_error(
            "--allow-sensitive is reserved for local-provider setups; this build only supports Cerebras and will not send sensitive files off-machine",
        ));
    }
    let start = Instant::now();
    let root =
        fs::canonicalize(&dir).map_err(|err| usage_error(format!("cannot read {dir}: {err}")))?;
    let markdown_only = env::var("SCOUT_MARKDOWN_ONLY").ok().as_deref() == Some("1");
    let (files, mut skipped) = walk_corpus(&root, false, markdown_only)?;
    let model = env::var("SCOUT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let old = open_snapshot(&root)
        .ok()
        .and_then(|s| load_cards(&s).ok().map(|c| (s.manifest, c)));
    let can_reuse = old
        .as_ref()
        .is_some_and(|(m, _)| manifest_compatible(m, &model, &root));
    let old_by_key: HashMap<(String, String), Card> = old
        .as_ref()
        .map(|(_, cards)| {
            cards
                .iter()
                .map(|c| ((c.path.clone(), c.hash.clone()), c.clone()))
                .collect()
        })
        .unwrap_or_default();
    let estimate_files: Vec<WalkedFile> = files_to_card(&files, can_reuse, &old_by_key)
        .into_iter()
        .cloned()
        .collect();
    let estimate = estimate_index_cost(&estimate_files);
    eprintln!(
        "scout index estimate before spend: {} files to card, ~${:.4}",
        estimate_files.len(),
        estimate
    );
    if let Some(cap) = max_dollars
        && estimate > cap
    {
        let mut env = Envelope::new("index", "budget_hit");
        env.root = Some(root.display().to_string());
        env.skipped = skipped;
        env.data = json!({"estimated_usd": estimate, "max_dollars": cap});
        return finish_env(&env, pretty);
    }
    if !yes && estimate > 0.0 && io::stdin().is_terminal() {
        eprint!("Proceed with index spend? [y/N] ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            let mut env = Envelope::new("index", "budget_hit");
            env.root = Some(root.display().to_string());
            env.data = json!({"cancelled": true, "estimated_usd": estimate});
            return finish_env(&env, pretty);
        }
    }

    let spend = Arc::new(Mutex::new(Spend::default()));
    let client = CerebrasClient::from_env(spend.clone())?;
    let gate = BudgetGate::new(max_dollars);
    let churn = git_churn(&root);
    let file_meta = manifest_file_meta(&root, &files);
    let mut cards = Vec::new();
    let mut unsupported = Vec::new();
    for file in files {
        if matches!(file.adapter.as_str(), "pdf" | "docx") && file.text.is_empty() {
            unsupported.push(Skip {
                path: file.rel,
                reason: "unsupported_or_tool_missing".into(),
                adapter: Some(file.adapter),
            });
            continue;
        }
        let skeleton = skeletonize(&file, *churn.get(&file.rel).unwrap_or(&0));
        if can_reuse && let Some(old_card) = old_by_key.get(&(file.rel.clone(), file.hash.clone()))
        {
            cards.push(Card {
                role: ModelHint {
                    model_hint: true,
                    value: card_role(&old_card.role.value),
                },
                invariants: old_card.invariants.clone(),
                gotchas: old_card.gotchas.clone(),
                terms: old_card.terms.clone(),
                ..skeleton
            });
            continue;
        }
        let path = file.rel.clone();
        let adapter = Some(file.adapter.clone());
        if gate.hit().is_some() {
            skipped.push(Skip {
                path,
                reason: "budget_hit".into(),
                adapter,
            });
        } else {
            record_card_result(
                &mut cards,
                &mut skipped,
                path,
                adapter,
                generate_card(&client, &gate, &file)
                    .map(|model| card_from_generated(skeleton, model)),
            );
        }
    }
    skipped.extend(unsupported.clone());
    let manifest = Manifest {
        schema_version: CARD_SCHEMA_VERSION,
        card_prompt_hash: sha256(CARD_PROMPT.as_bytes()),
        model: client.model.clone(),
        adapter_version: adapter_version(),
        ignore_config_hash: ignore_config_hash(&root),
        generated_at_unix_ms: unix_ms(),
        root: root.display().to_string(),
        cards: cards.len(),
        markdown_only,
        file_meta,
        skipped: skipped.clone(),
        unsupported,
    };
    let state = index_state_after_card_generation(&cards, &skipped, &manifest.unsupported);
    if matches!(state, "provider_error" | "budget_hit") {
        let mut env = Envelope::new("index", state);
        env.root = Some(root.display().to_string());
        env.spend = spend.lock().unwrap().clone();
        env.skipped = skipped;
        env.errors.push(if state == "budget_hit" {
            "budget cap hit before any cards were generated".into()
        } else {
            "all card generation attempts failed".into()
        });
        env.timings_ms
            .insert("total".into(), start.elapsed().as_millis());
        env.data = json!({
            "cards": 0,
            "estimated_usd": estimate,
            "unsupported": coverage_skips(&manifest),
        });
        return finish_env(&env, pretty);
    }
    let gen_dir = write_generation(&root, &manifest, &cards)?;
    prune_generations(&root, 2).ok();
    let mut env = Envelope::new("index", state);
    env.root = Some(root.display().to_string());
    env.spend = spend.lock().unwrap().clone();
    env.skipped = skipped;
    env.timings_ms
        .insert("total".into(), start.elapsed().as_millis());
    env.data = json!({
        "generation": gen_dir.file_name().unwrap().to_string_lossy(),
        "cards": manifest.cards,
        "estimated_usd": estimate,
        "unsupported": coverage_skips(&manifest),
    });
    finish_env(&env, pretty)
}

#[derive(Deserialize, Default)]
struct GeneratedCard {
    #[serde(default)]
    role: String,
    #[serde(default)]
    invariants: Vec<String>,
    #[serde(default)]
    gotchas: Vec<String>,
    #[serde(default)]
    terms: Vec<String>,
}

fn generate_card(
    client: &CerebrasClient,
    gate: &BudgetGate,
    file: &WalkedFile,
) -> Result<GeneratedCard> {
    let text = truncate_chars(&redact_outbound(&file.text).0, 16_000);
    let prompt = format!(
        "FILE: {}\nADAPTER: {}\n\n{}\n\nReturn ONLY JSON: {{\"role\": \"<=20 word one-line file role\", \"invariants\": [\"short hints\"], \"gotchas\": [\"short hints\"], \"terms\": [\"defined terms\"]}}. These fields are routing hints, not delivered facts. If unsure, use empty arrays.",
        file.rel, file.adapter, text
    );
    if !gate.may_launch(&client.spend, estimate_chat_cost(&prompt, 900)) {
        return Err(budget_hit_error("budget cap hit before card generation"));
    }
    let raw = client
        .chat_with_budget(
            vec![
                Message::system("You write compact scout index card hints. Do not synthesize user-facing facts. Return strict JSON only."),
                Message::user(prompt),
            ],
            900,
            Some(gate),
        )
        .map_err(|err| {
            if err.downcast_ref::<BudgetHitError>().is_some() {
                err
            } else {
                provider_error(format!("{err:#}"))
            }
        })?;
    let value: GeneratedCard = parse_jsonish(&raw).unwrap_or_else(|_| GeneratedCard {
        role: deterministic_role(&file.rel, &file.text),
        ..GeneratedCard::default()
    });
    Ok(GeneratedCard {
        role: card_role(&value.role),
        invariants: value.invariants.into_iter().take(8).collect(),
        gotchas: value.gotchas.into_iter().take(8).collect(),
        terms: value.terms.into_iter().take(12).collect(),
    })
}

fn card_from_generated(skeleton: Card, model: GeneratedCard) -> Card {
    Card {
        role: ModelHint {
            model_hint: true,
            value: card_role(&model.role),
        },
        invariants: ModelHint {
            model_hint: true,
            value: model.invariants,
        },
        gotchas: ModelHint {
            model_hint: true,
            value: model.gotchas,
        },
        terms: ModelHint {
            model_hint: true,
            value: model.terms,
        },
        ..skeleton
    }
}

fn record_card_result(
    cards: &mut Vec<Card>,
    skipped: &mut Vec<Skip>,
    path: String,
    adapter: Option<String>,
    result: Result<Card>,
) {
    match result {
        Ok(card) => cards.push(card),
        Err(err) => {
            let reason = if err.downcast_ref::<BudgetHitError>().is_some() {
                "budget_hit".into()
            } else {
                format!("provider_error: {}", brief_error(&err))
            };
            skipped.push(Skip {
                path,
                reason,
                adapter,
            });
        }
    }
}

fn brief_error(err: &anyhow::Error) -> String {
    truncate_chars(&format!("{err:#}").replace('\n', " "), 200)
}

fn index_state_after_card_generation(
    cards: &[Card],
    skipped: &[Skip],
    unsupported: &[Skip],
) -> &'static str {
    let provider_failures = skipped
        .iter()
        .any(|s| s.reason.starts_with("provider_error:"));
    let budget_failures = skipped.iter().any(|s| s.reason == "budget_hit");
    if cards.is_empty() && provider_failures {
        "provider_error"
    } else if cards.is_empty() && budget_failures {
        "budget_hit"
    } else if budget_failures || provider_failures {
        "partial"
    } else if unsupported
        .iter()
        .any(|s| s.reason == "unsupported_or_tool_missing")
    {
        "tool_degraded"
    } else {
        "ok"
    }
}

#[derive(Debug, Clone)]
struct RefreshResult {
    spend: Spend,
    estimated_usd: f64,
    generation: String,
}

enum RefreshDecision {
    NotNeeded,
    Refreshed(RefreshResult),
    Refused(Envelope),
}

fn refresh_index_if_needed(
    command: &str,
    root: &Path,
    max_dollars: Option<f64>,
    markdown_only: bool,
) -> Result<RefreshDecision> {
    let current = open_snapshot(root)
        .ok()
        .and_then(|snapshot| load_cards(&snapshot).ok().map(|cards| (snapshot, cards)));
    let stale = current
        .as_ref()
        .map(|(snapshot, cards)| staleness_report(root, &snapshot.manifest, cards))
        .transpose()?;
    if stale.as_ref().is_some_and(|s| !s.stale) {
        return Ok(RefreshDecision::NotNeeded);
    }
    let estimate = incremental_index_estimate(root, markdown_only)?;
    if let Some(cap) = max_dollars
        && estimate.usd > cap
    {
        let mut env = Envelope::new(command, "budget_hit");
        env.root = Some(root.display().to_string());
        env.data = json!({
            "hint": "run scout index --max-dollars with a higher cap",
            "estimated_usd": estimate.usd,
            "max_dollars": cap,
            "changed_files": estimate.files_to_card,
        });
        return Ok(RefreshDecision::Refused(env));
    }
    if estimate.usd > REFRESH_AUTO_YES_MAX_USD {
        let mut env = Envelope::new(command, "budget_hit");
        env.root = Some(root.display().to_string());
        env.data = json!({
            "hint": "refresh estimate exceeds auto-confirm threshold; run scout index --yes first",
            "estimated_usd": estimate.usd,
            "auto_yes_max_usd": REFRESH_AUTO_YES_MAX_USD,
            "changed_files": estimate.files_to_card,
        });
        return Ok(RefreshDecision::Refused(env));
    }
    let spend = cmd_index_internal(&root.display().to_string(), max_dollars, markdown_only)?;
    let generation = open_snapshot(root)
        .ok()
        .and_then(|s| {
            s.gen_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    Ok(RefreshDecision::Refreshed(RefreshResult {
        spend,
        estimated_usd: estimate.usd,
        generation,
    }))
}

fn cmd_query(args: &[String]) -> Result<i32> {
    let query = args[0].clone();
    let mut dir = ".".to_string();
    let mut budget_tokens = None;
    let mut max_dollars = None;
    let mut pretty = false;
    let mut refresh = false;
    let mut compact = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--budget" => {
                i += 1;
                budget_tokens = Some(parse_budget(arg_value(args, i, "--budget")?)?);
            }
            "--max-dollars" => {
                i += 1;
                max_dollars = Some(parse_money_arg(
                    arg_value(args, i, "--max-dollars")?,
                    "--max-dollars",
                )?);
            }
            "--pretty" => pretty = true,
            "--refresh" => refresh = true,
            "--compact" => compact = true,
            s if !s.starts_with('-') => dir = s.to_string(),
            s => return Err(usage_error(format!("unknown query flag {s}"))),
        }
        i += 1;
    }
    let root = fs::canonicalize(&dir).unwrap_or_else(|_| PathBuf::from(dir));
    let refresh_result = if refresh {
        match refresh_index_if_needed("query", &root, max_dollars, false)? {
            RefreshDecision::NotNeeded => None,
            RefreshDecision::Refreshed(result) => Some(result),
            RefreshDecision::Refused(env) => return finish_env(&env, pretty),
        }
    } else {
        None
    };
    let query_max_dollars =
        max_dollars.map(|cap| cap - refresh_result.as_ref().map(|r| r.spend.usd).unwrap_or(0.0));
    match query_once(&query, &root, query_max_dollars) {
        Ok((env, code)) => {
            let mut env = env;
            if let Some(result) = refresh_result {
                env.spend.merge(&result.spend);
                insert_data_field(
                    &mut env.data,
                    "refreshed_index",
                    json!({"estimated_usd": result.estimated_usd, "generation": result.generation}),
                );
            }
            persist_query_artifacts(&root, &mut env)?;
            let emitted = finish_env(&query_stdout_envelope(&env, budget_tokens, compact), pretty)?;
            debug_assert_eq!(emitted, code);
            Ok(emitted)
        }
        Err(err) if err.to_string().contains("index_missing") => {
            let mut env = Envelope::new("query", "index_missing");
            env.root = Some(root.display().to_string());
            env.errors.push("index_missing; run scout index".into());
            finish_env(&env, pretty)
        }
        Err(err) => Err(err),
    }
}

fn query_once(query: &str, root: &Path, max_dollars: Option<f64>) -> Result<(Envelope, i32)> {
    let total_start = Instant::now();
    let snapshot = open_snapshot(root).map_err(|_| anyhow!("index_missing"))?;
    let cards = load_cards(&snapshot)?;
    let stale =
        staleness_report(root, &snapshot.manifest, &cards).unwrap_or_else(|_| StalenessReport {
            stale: true,
            reason: "content_hash_mismatch".into(),
            changed_files: 0,
        });
    if stale.stale {
        let mut env = Envelope::new("query", "index_stale");
        env.root = Some(root.display().to_string());
        env.data = json!({
            "hint": "run scout index --refresh",
            "reason": stale.reason,
            "changed_files": stale.changed_files,
        });
        return Ok(with_exit_code(env));
    }
    let files = read_card_files(root, &cards);
    let deterministic = deterministic_candidates(query, &cards, &files, DEFAULT_CANDIDATES);
    let router_estimate = estimate_tokens(&thin_cards_json(query, &cards)) as f64 / 1_000_000.0
        * INPUT_DOLLARS_PER_MTOK
        + 0.003;
    eprintln!(
        "scout query estimate before router spend: ~${:.4}",
        router_estimate
    );
    if max_dollars.is_some_and(|cap| router_estimate > cap) {
        let mut env = Envelope::new("query", "budget_hit");
        env.root = Some(root.display().to_string());
        env.data = json!({"estimated_usd": router_estimate, "max_dollars": max_dollars});
        return Ok(with_exit_code(env));
    }
    let spend = Arc::new(Mutex::new(Spend::default()));
    let client = CerebrasClient::from_env(spend.clone())?;
    let gate = BudgetGate::new(max_dollars);
    let router = match route_with_model(&client, &gate, query, &cards) {
        Ok(router) => router,
        Err(err) if err.downcast_ref::<BudgetHitError>().is_some() => return Err(err),
        Err(err) => {
            eprintln!("router degraded to deterministic union: {err:#}");
            Vec::new()
        }
    };
    let candidates = final_candidates(router, deterministic, &cards);
    let candidate_paths: Vec<String> = candidates.iter().map(|c| c.path.clone()).collect();
    if candidates.is_empty() {
        let mut env = Envelope::new("query", "unanswered");
        env.root = Some(root.display().to_string());
        env.spend = spend.lock().unwrap().clone();
        env.timings_ms
            .insert("total".into(), total_start.elapsed().as_millis());
        env.data = json!({
            "query": query,
            "generation": snapshot.gen_dir.file_name().unwrap().to_string_lossy(),
            "findings": [],
            "dropped": [],
            "candidate_files": [],
            "weak_signal": false,
        });
        return Ok(with_exit_code(env));
    }
    let mode = env::var("SCOUT_CHUNK_MODE").unwrap_or_else(|_| "ranked_boundary".into());
    let chunks = build_chunks(&candidates, &files, &mode, query);
    let extractor_estimate = estimate_chunks_cost(&chunks);
    eprintln!(
        "scout query estimate before extractor spend: {} chunks, ~${:.4}",
        chunks.len(),
        extractor_estimate
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(
            env::var("SCOUT_CONCURRENCY")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_CONCURRENCY),
        )
        .build()?;
    let results: Vec<Result<Vec<Finding>, DroppedFinding>> = pool.install(|| {
        chunks
            .par_iter()
            .map(|chunk| extract_chunk(&client, &gate, query, chunk))
            .collect()
    });
    let mut raw = Vec::new();
    let mut dropped = Vec::new();
    for result in results {
        match result {
            Ok(mut findings) => raw.append(&mut findings),
            Err(err) => dropped.push(err),
        }
    }
    let (verified, mut quote_dropped) = verify_quotes(raw, &files);
    dropped.append(&mut quote_dropped);
    let mut findings = Vec::new();
    for finding in verified {
        if matches_exact_claim_query(query, &finding) {
            findings.push(finding);
        } else {
            dropped.push(drop_reason(&finding, "partial_claim_match"));
        }
    }
    findings.sort_by(sort_findings);
    findings.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.quote == b.quote);
    let weak_signal = is_weak_signal(&findings, top_candidate_deterministic_score(&candidates));
    let budget_hit = gate.hit();
    let state = query_state(budget_hit.is_some(), chunks.len(), findings.len(), &dropped);
    let mut env = Envelope::new("query", state);
    env.root = Some(root.display().to_string());
    env.spend = spend.lock().unwrap().clone();
    env.skipped = snapshot.manifest.skipped.clone();
    env.timings_ms
        .insert("total".into(), total_start.elapsed().as_millis());
    env.data = json!({
        "query": query,
        "generation": snapshot.gen_dir.file_name().unwrap().to_string_lossy(),
        "candidate_files": candidate_paths,
        "chunks": chunks.len(),
        "chunk_mode": mode,
        "findings": findings,
        "dropped": dropped,
        "weak_signal": weak_signal,
        "budget_hit": budget_hit,
    });
    let code = exit_code_for_state(&env.state);
    Ok((env, code))
}

fn query_state(
    budget_hit: bool,
    chunks: usize,
    findings: usize,
    dropped: &[DroppedFinding],
) -> &'static str {
    if budget_hit {
        return "budget_hit";
    }
    if chunks > 0
        && findings == 0
        && !dropped.is_empty()
        && dropped
            .iter()
            .all(|d| d.reason.starts_with("provider_error"))
    {
        return "provider_error";
    }
    if dropped
        .iter()
        .any(|d| d.reason.starts_with("provider_error") || d.reason.starts_with("budget_hit"))
    {
        return "partial";
    }
    if findings == 0 { "unanswered" } else { "ok" }
}

fn extract_chunk(
    client: &CerebrasClient,
    gate: &BudgetGate,
    query: &str,
    chunk: &Chunk,
) -> Result<Vec<Finding>, DroppedFinding> {
    let projected =
        estimate_tokens(&chunk.body) as f64 / 1_000_000.0 * INPUT_DOLLARS_PER_MTOK + 0.006;
    if !gate.may_launch(&client.spend, projected) {
        return Err(DroppedFinding {
            file: chunk.file.clone(),
            line: Some(chunk.first_line),
            fact: None,
            quote: None,
            reason: "budget_hit_before_chunk".into(),
        });
    }
    let (redacted, redactions) = redact_outbound(&chunk.body);
    let guidance = chunk.look_for.clone().unwrap_or_default();
    let user = format!(
        "FILE: {} (chunk starting at line {})\nLOOK_FOR: {}\nREDACTIONS_APPLIED: {}\n\n{}\n\nQUERY: {}",
        chunk.file, chunk.first_line, guidance, redactions, redacted, query
    );
    let raw = client
        .chat_with_budget(
            vec![Message::system(EXTRACTOR_SYS), Message::user(user)],
            2000,
            Some(gate),
        )
        .map_err(|err| DroppedFinding {
            file: chunk.file.clone(),
            line: Some(chunk.first_line),
            fact: None,
            quote: None,
            reason: if err.downcast_ref::<BudgetHitError>().is_some() {
                "budget_hit_before_retry".into()
            } else {
                format!("provider_error: {err:#}")
            },
        })?;
    let arr: Vec<RawFinding> = parse_jsonish(&raw).map_err(|err| DroppedFinding {
        file: chunk.file.clone(),
        line: Some(chunk.first_line),
        fact: None,
        quote: Some(raw.chars().take(500).collect()),
        reason: format!("unparseable: {err}"),
    })?;
    Ok(arr
        .into_iter()
        .filter_map(|f| {
            Some(Finding {
                file: chunk.file.clone(),
                line: f.line?,
                fact: f.fact?,
                quote: f.quote,
                quote_omitted: false,
                router_rank: chunk.router_rank,
                deterministic_score: chunk.deterministic_score,
                match_tier: None,
            })
        })
        .collect())
}

const EXTRACTOR_SYS: &str = r#"You are a code/document exploration extractor. You get ONE chunk of ONE file with line numbers and a query. Return ONLY a JSON array of findings relevant to the query, found IN THIS CHUNK. Each finding: {"fact":"one precise sentence","line":123,"quote":"verbatim source quote"}.
Rules:
- quote must be copied verbatim from the chunk without the line-number prefix; it will be machine-checked.
- fact must be fully supported by the quote and immediate context. Do not infer from unseen files.
- Only include findings genuinely relevant to the query.
- If nothing in this chunk is relevant, return []. That is correct for most chunks. Never invent a finding.
Return JSON only, no prose or markdown fence."#;

fn cmd_brief(args: &[String]) -> Result<i32> {
    let mut dir = ".".to_string();
    let mut budget_tokens = None;
    let mut max_dollars = None;
    let mut pretty = false;
    let mut refresh = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--budget" => {
                i += 1;
                budget_tokens = Some(parse_budget(arg_value(args, i, "--budget")?)?);
            }
            "--max-dollars" => {
                i += 1;
                max_dollars = Some(parse_money_arg(
                    arg_value(args, i, "--max-dollars")?,
                    "--max-dollars",
                )?);
            }
            "--pretty" => pretty = true,
            "--refresh" => refresh = true,
            s if !s.starts_with('-') => dir = s.to_string(),
            other => return Err(usage_error(format!("unknown brief flag {other}"))),
        }
        i += 1;
    }
    let root = fs::canonicalize(&dir).unwrap_or_else(|_| PathBuf::from(dir));
    let refresh_result = if refresh {
        match refresh_index_if_needed("brief", &root, max_dollars, false)? {
            RefreshDecision::NotNeeded => None,
            RefreshDecision::Refreshed(result) => Some(result),
            RefreshDecision::Refused(env) => return finish_env(&env, pretty),
        }
    } else {
        None
    };
    let snapshot = match open_snapshot(&root) {
        Ok(s) => s,
        Err(_) => {
            let mut env = Envelope::new("brief", "index_missing");
            env.root = Some(root.display().to_string());
            env.data = json!({"hint": "run scout index"});
            return finish_env(&env, pretty);
        }
    };
    let mut cards = load_cards(&snapshot)?;
    cards.sort_by(|a, b| a.path.cmp(&b.path));
    let stale =
        staleness_report(&root, &snapshot.manifest, &cards).unwrap_or_else(|_| StalenessReport {
            stale: true,
            reason: "content_hash_mismatch".into(),
            changed_files: 0,
        });
    let mut dirs: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for c in &cards {
        let dir = Path::new(&c.path)
            .parent()
            .map(|p| p.display().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".into());
        dirs.entry(dir).or_default().push(json!({
            "path": c.path,
            "role": c.role,
            "symbols": c.symbols.iter().take(8).collect::<Vec<_>>(),
            "outline": c.outline.iter().take(8).collect::<Vec<_>>(),
            "loc": c.loc,
            "churn": c.churn,
            "harness_meta": c.harness_meta,
        }));
    }
    let entry_points = brief_entry_points(&cards);
    let unsupported = coverage_skips(&snapshot.manifest);
    let stale_reason = stale.stale.then(|| stale.reason.clone());
    let data = json!({
        "generation": snapshot.gen_dir.file_name().unwrap().to_string_lossy(),
        "module_map": dirs,
        "entry_points": entry_points.into_iter().take(20).map(|c| json!({"path": c.path, "role": c.role, "imports": c.imports.len(), "churn": c.churn, "harness_meta": c.harness_meta})).collect::<Vec<_>>(),
        "unsupported": unsupported,
        "coverage_note": coverage_note(&unsupported),
        "stale": stale.stale,
        "stale_reason": stale_reason,
        "changed_files": stale.changed_files,
    });
    let packed = if let Some(tokens) = budget_tokens {
        truncate_json_value(data, tokens)
    } else {
        data
    };
    let mut env = Envelope::new("brief", "ok");
    env.root = Some(root.display().to_string());
    env.skipped = snapshot.manifest.skipped.clone();
    env.data = packed;
    if let Some(result) = refresh_result {
        env.spend.merge(&result.spend);
        insert_data_field(
            &mut env.data,
            "refreshed_index",
            json!({"estimated_usd": result.estimated_usd, "generation": result.generation}),
        );
    }
    finish_env(&env, pretty)
}

fn brief_entry_points(cards: &[Card]) -> Vec<Card> {
    let mut entry_points = cards.to_vec();
    entry_points.sort_by(|a, b| {
        a.harness_meta
            .cmp(&b.harness_meta)
            .then_with(|| {
                (b.imports.len() as u32 + b.churn).cmp(&(a.imports.len() as u32 + a.churn))
            })
            .then_with(|| a.path.cmp(&b.path))
    });
    entry_points
}

fn cmd_capabilities(args: &[String]) -> Result<i32> {
    let pretty = parse_pretty_only(args, "capabilities")?;
    let mut env = Envelope::new("capabilities", "ok");
    env.data = json!({
        "provider": "cerebras",
        "default_model": DEFAULT_MODEL,
        "env_keys": ["CEREBRAS_API_KEY", "SCOUT_API_KEY", "SCOUT_MODEL"],
        "content_leaves_machine": ["index card prompts for non-sensitive files", "router thin cards", "redacted extraction chunks"],
        "sensitive_deny": [".env*", "*.env", "id_rsa*", "id_ed25519*", "id_ecdsa*", "id_dsa*", ".ssh/", ".aws/", ".config/gcloud/", ".kube/", ".docker/", ".gnupg/", ".azure/", ".netrc", ".npmrc", ".pypirc", ".pgpass", ".htpasswd", "*.pem", "*.key", "*.p12", "*.crt", "*.cer", "*.pfx", "*.jks", "*.keystore", "*.ppk", "*.asc", "*.gpg"],
        "redaction": ["known token formats", "bearer tokens", "key/value secrets", "JWT", "high-entropy strings", "emails", "home paths"],
        "adapters": {"code": "regex skeleton; ctags optional doctor signal", "markdown": "headings/links", "pdf": "pdftotext if installed", "docx": "pandoc if installed"},
        "states": state_names(),
        "exit_codes": exit_codes_json(),
        "flags": {
            "query": ["--budget", "--max-dollars", "--refresh", "--compact", "--pretty", "--query"],
            "brief": ["--budget", "--max-dollars", "--refresh", "--pretty"],
            "index": ["--yes", "--max-dollars", "--pretty"]
        },
        "query_artifacts": { "last_run": ".scout/last-run.json", "history": ".scout/runs/*.json", "kept": 20 },
    });
    finish_env(&env, pretty)
}

fn cmd_schema(args: &[String]) -> Result<i32> {
    let pretty = parse_pretty_only(args, "schema")?;
    let mut env = Envelope::new("schema", "ok");
    env.data = json!({
        "envelope": {"schema": SCHEMA, "state": state_names().join("|"), "data": "command-specific"},
        "exit_codes": exit_codes_json(),
        "card_schema_version": CARD_SCHEMA_VERSION,
        "finding": {"file": "relative path", "line": "1-based source line", "fact": "model text supported by quote", "quote": "verbatim unless omitted", "quote_omitted": "true when budget packed", "match_tier": "exact|markdown_normalized", "deterministic_score": "omitted only with --compact", "router_rank": "omitted only with --compact"},
        "query_data": {"weak_signal": "true when max finding deterministic_score is below 40% of the top candidate deterministic score", "dropped": "array, or {reason: count} with --compact", "changed_files": "present on index_stale"},
        "brief_data": {"stale": "bool", "stale_reason": "null|manifest_mismatch|content_hash_mismatch", "changed_files": "files changed vs manifest"},
        "last_run": ".scout/last-run.json and .scout/runs/*.json contain full query envelopes",
    });
    finish_env(&env, pretty)
}

fn cmd_doctor(args: &[String]) -> Result<i32> {
    let pretty = parse_pretty_only(args, "doctor")?;
    let root = fs::canonicalize(".").unwrap_or_else(|_| PathBuf::from("."));
    let mut env = Envelope::new("doctor", "ok");
    let tools = json!({
        "ctags": command_exists("ctags"),
        "pdftotext": command_exists("pdftotext"),
        "pandoc": command_exists("pandoc"),
    });
    let has_key = env::var("CEREBRAS_API_KEY").is_ok() || env::var("SCOUT_API_KEY").is_ok();
    let index = open_snapshot(&root).ok();
    env.data = json!({
        "api_key_present": has_key,
        "model": env::var("SCOUT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
        "user_agent": USER_AGENT,
        "tools": tools,
        "index": index.as_ref().map(|s| json!({"generation": s.gen_dir.file_name().unwrap().to_string_lossy(), "cards": s.manifest.cards})),
        "degraded": {"code_symbols": !command_exists("ctags"), "pdf": !command_exists("pdftotext"), "docx": !command_exists("pandoc")},
    });
    if !has_key {
        env.state = "provider_error".into();
        env.errors.push("CEREBRAS_API_KEY missing".into());
    }
    finish_env(&env, pretty)
}

fn cmd_eval(args: &[String]) -> Result<i32> {
    let milestone = args.first().map(String::as_str).unwrap_or("m1");
    let mut max_dollars = Some(8.0);
    let mut only: Option<String> = None;
    let mut yes = false;
    let mut pretty = false;
    let mut corpus_override: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--max-dollars" => {
                i += 1;
                max_dollars = Some(parse_money_arg(
                    arg_value(args, i, "--max-dollars")?,
                    "--max-dollars",
                )?);
            }
            "--only" => {
                i += 1;
                only = Some(arg_value(args, i, "--only")?.to_string());
            }
            "--corpus" => {
                i += 1;
                corpus_override = Some(arg_value(args, i, "--corpus")?.to_string());
            }
            "--yes" | "-y" => yes = true,
            "--pretty" => pretty = true,
            other => return Err(usage_error(format!("unknown eval flag {other}"))),
        }
        i += 1;
    }
    let path = match milestone {
        "m1" => PathBuf::from("eval/goldens/code.json"),
        "m3" => PathBuf::from("eval/goldens/private/policy_markdown.json"),
        other => return Err(usage_error(format!("unknown eval milestone {other}"))),
    };
    let mut suite = load_eval_suite(&path).with_context(|| {
        format!(
            "missing eval suite {}; this milestone needs a user-supplied corpus and goldens file — see eval/README.md for the schema and setup",
            path.display()
        )
    })?;
    let corpus_override = corpus_override.or_else(|| env::var("SCOUT_EVAL_POLICY_CORPUS").ok());
    if let Some(root) = &corpus_override {
        for q in &mut suite.queries {
            q.corpus_root = root.clone();
        }
    }
    let queries: Vec<&EvalQuery> = suite
        .queries
        .iter()
        .filter(|q| only.as_ref().is_none_or(|id| &q.id == id))
        .collect();
    if queries.is_empty() {
        bail!("no eval queries matched");
    }
    let estimate = queries.len() as f64 * 0.15;
    eprintln!(
        "scout eval {milestone} predeclared estimate before spend: {} queries, cap {:?}, projected <= ${:.2}",
        queries.len(),
        max_dollars,
        estimate
    );
    if !yes && io::stdin().is_terminal() {
        eprint!("Proceed with live eval spend? [y/N] ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            bail!("eval cancelled before spend");
        }
    }
    let mut total_spend = Spend::default();
    let mut results = Vec::new();
    let cap = max_dollars.unwrap_or(8.0);
    let eval_dir = PathBuf::from(".scout")
        .join("eval-runs")
        .join(format!("{milestone}-{}", unix_ms()));
    fs::create_dir_all(&eval_dir)?;
    for q in &queries {
        if total_spend.usd >= cap {
            break;
        }
        let index_spend = cmd_index_internal(
            &q.corpus_root,
            Some(cap - total_spend.usd),
            milestone == "m3",
        )?;
        total_spend.merge(&index_spend);
        if total_spend.usd >= cap {
            break;
        }
        let (mut env, _code) = query_once(
            &q.query,
            Path::new(&q.corpus_root),
            Some(cap - total_spend.usd),
        )?;
        persist_query_artifacts(Path::new(&q.corpus_root), &mut env)?;
        total_spend.merge(&env.spend);
        let findings: Vec<Finding> = serde_json::from_value(
            env.data
                .get("findings")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .unwrap_or_default();
        let candidate_files: Vec<String> = serde_json::from_value(
            env.data
                .get("candidate_files")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .unwrap_or_default();
        let must_files = q
            .must_find
            .iter()
            .filter(|g| candidate_files.contains(&g.file))
            .count();
        let covered = q
            .must_find
            .iter()
            .filter(|g| finding_covers(g, &findings))
            .count();
        let poison_survivors = q
            .poison
            .iter()
            .filter(|p| poison_survives(&p.fact, &q.query, &findings))
            .count();
        let artifact_path = write_eval_artifact(&eval_dir, q, &env)?;
        results.push(json!({
            "id": q.id,
            "state": env.state,
            "must_files_present": must_files,
            "must_files_total": q.must_find.len(),
            "coverage": covered,
            "coverage_total": q.must_find.len(),
            "poison_survivors": poison_survivors,
            "negative_clean": q.negative.unwrap_or(false) && findings.is_empty(),
            "spend_usd": env.spend.usd,
            "candidate_files": candidate_files,
            "artifact_path": artifact_path,
        }));
        if total_spend.usd >= cap {
            break;
        }
    }
    let must_total: usize = queries.iter().map(|q| q.must_find.len()).sum();
    let covered_total: usize = results
        .iter()
        .map(|r| r.get("coverage").and_then(Value::as_u64).unwrap_or(0) as usize)
        .sum();
    let file_total: usize = queries.iter().map(|q| q.must_find.len()).sum();
    let file_present: usize = results
        .iter()
        .map(|r| {
            r.get("must_files_present")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        })
        .sum();
    let poison_total: usize = results
        .iter()
        .map(|r| {
            r.get("poison_survivors")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        })
        .sum();
    let avg_query = if results.is_empty() {
        0.0
    } else {
        total_spend.usd / results.len() as f64
    };
    let negative_ids: HashSet<&str> = queries
        .iter()
        .filter(|q| q.negative.unwrap_or(false))
        .map(|q| q.id.as_str())
        .collect();
    let negatives_clean = results.iter().all(|r| {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("");
        !negative_ids.contains(id)
            || r.get("negative_clean")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    let suite_complete = results.len() == queries.len();
    let coverage_pass = must_total == 0 || covered_total as f64 / must_total as f64 >= 0.714;
    let candidate_file_recall_pass = file_total == file_present;
    let poison_pass = poison_total == 0;
    let cost_pass = avg_query < 0.15;
    let green = suite_complete
        && candidate_file_recall_pass
        && coverage_pass
        && poison_pass
        && negatives_clean
        && cost_pass;
    let metrics = json!({
        "candidate_file_recall": pct(file_present, file_total),
        "coverage": pct(covered_total, must_total),
        "poison_survivors": poison_total,
        "negatives_clean": negatives_clean,
        "suite_complete": suite_complete,
        "avg_usd_per_query": avg_query,
    });
    let gate_results = json!({
        "candidate_file_recall": candidate_file_recall_pass,
        "coverage": coverage_pass,
        "poison_survivors": poison_pass,
        "negatives_clean": negatives_clean,
        "suite_complete": suite_complete,
        "avg_usd_per_query": cost_pass,
    });
    let summary_path = eval_dir.join("summary.json");
    atomic_write(
        &summary_path,
        &serde_json::to_vec_pretty(&json!({
            "milestone": milestone,
            "metrics": metrics.clone(),
            "gate_results": gate_results.clone(),
            "total_spend": total_spend.clone(),
            "per_query_artifacts": results.iter().map(|r| json!({
                "id": r.get("id"),
                "state": r.get("state"),
                "artifact_path": r.get("artifact_path"),
                "candidate_file_recall_pass": r.get("must_files_present") == r.get("must_files_total"),
                "coverage_pass": r.get("coverage") == r.get("coverage_total"),
                "poison_pass": r.get("poison_survivors").and_then(Value::as_u64).unwrap_or(0) == 0,
                "negative_clean": r.get("negative_clean"),
            })).collect::<Vec<_>>(),
        }))?,
    )?;
    let mut env = Envelope::new("eval", if green { "ok" } else { "partial" });
    env.spend = total_spend;
    env.data = json!({
        "milestone": milestone,
        "results": results,
        "metrics": metrics,
        "bars": {"candidate_file_recall": "100%", "coverage": ">=71.4%", "poison_survivors": 0, "avg_usd_per_query": "<0.15"},
        "gate_results": gate_results,
        "eval_run_dir": eval_dir,
        "summary_path": summary_path,
    });
    finish_env(&env, pretty)
}

fn cmd_index_internal(root: &str, max_dollars: Option<f64>, markdown_only: bool) -> Result<Spend> {
    let mut cmd = Command::new(env::current_exe()?);
    cmd.arg("index").arg(root).arg("--yes");
    if markdown_only {
        cmd.env("SCOUT_MARKDOWN_ONLY", "1");
    }
    if let Some(cap) = max_dollars {
        cmd.arg("--max-dollars").arg(format!("{cap:.6}"));
    }
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    let env: Envelope = serde_json::from_slice(&output.stdout)?;
    if let Some(err) = index_subprocess_error(output.status.code(), &env, &output.stdout) {
        return Err(err);
    }
    Ok(env.spend)
}

fn index_subprocess_failed(status_code: Option<i32>, state: &str) -> bool {
    !matches!(
        (status_code, state),
        (Some(0), _) | (Some(14), "tool_degraded")
    )
}

fn index_subprocess_error(
    status_code: Option<i32>,
    env: &Envelope,
    stdout: &[u8],
) -> Option<anyhow::Error> {
    if !index_subprocess_failed(status_code, &env.state) {
        return None;
    }
    let msg = format!(
        "index failed with status {:?}; stdout: {}",
        status_code,
        String::from_utf8_lossy(stdout)
    );
    Some(match env.state.as_str() {
        "provider_error" => provider_error(msg),
        "budget_hit" => budget_hit_error(msg),
        _ => anyhow!(msg),
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct EvalSuite {
    queries: Vec<EvalQuery>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EvalQuery {
    id: String,
    corpus_root: String,
    query: String,
    #[serde(default)]
    must_find: Vec<GoldenFact>,
    #[serde(default)]
    poison: Vec<GoldenPoison>,
    #[serde(default)]
    negative: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GoldenFact {
    fact: String,
    file: String,
    line: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct GoldenPoison {
    fact: String,
}

fn load_eval_suite(path: &Path) -> Result<EvalSuite> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn write_eval_artifact(dir: &Path, query: &EvalQuery, env: &Envelope) -> Result<String> {
    let last_run = env
        .data
        .get("last_run_path")
        .and_then(Value::as_str)
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let path = dir.join(format!("{}.json", query.id));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "query": query,
            "envelope": env,
            "last_run": last_run,
        }))?,
    )?;
    Ok(path.display().to_string())
}

fn finding_covers(g: &GoldenFact, findings: &[Finding]) -> bool {
    let expected_terms: HashSet<String> = terms(&g.fact).into_iter().collect();
    findings.iter().any(|f| {
        if f.file != g.file {
            return false;
        }
        if f.line.abs_diff(g.line) <= 5 {
            return true;
        }
        let actual_terms: HashSet<String> = terms(&f.fact).into_iter().collect();
        !expected_terms.is_empty()
            && expected_terms.intersection(&actual_terms).count() as f64
                / expected_terms.len() as f64
                > 0.5
    })
}

fn poison_survives(poison: &str, query: &str, findings: &[Finding]) -> bool {
    let poison_terms: HashSet<String> = terms(poison).into_iter().collect();
    if poison_terms.is_empty() {
        return false;
    }
    let query_terms: HashSet<String> = terms(query).into_iter().collect();
    let poison_low = poison.to_ascii_lowercase();
    let required_terms = required_poison_terms(poison);
    let relation_terms = HashSet::from([
        "ask", "claim", "cover", "creat", "describe", "include", "make", "mention", "say", "state",
    ]);
    let distinctive_terms: HashSet<String> = poison_terms
        .iter()
        .filter(|term| {
            !query_terms.contains(*term)
                && !required_terms.contains(*term)
                && !relation_terms.contains(term.as_str())
        })
        .cloned()
        .collect();
    let poison_has_restrictor = has_restrictor(poison);
    findings.iter().any(|f| {
        let fact_low = f.fact.to_ascii_lowercase();
        let counterclaim = [
            "does not",
            "doesn't",
            "not ",
            "no ",
            "never",
            "without",
            "rather than",
            "instead of",
        ]
        .iter()
        .any(|needle| fact_low.contains(needle))
            && !poison_low.contains("not");
        if counterclaim {
            return false;
        }
        let ft: HashSet<String> = terms(&f.fact).into_iter().collect();
        if !required_terms.is_subset(&ft) {
            return false;
        }
        if poison_has_restrictor && !has_restrictor(&f.fact) {
            return false;
        }
        if !distinctive_terms.is_empty()
            && distinctive_terms
                .iter()
                .filter(|term| ft.contains(*term))
                .count()
                < distinctive_terms.len().min(2)
        {
            return false;
        }
        poison_terms.intersection(&ft).count() as f64 / poison_terms.len() as f64 > 0.6
    })
}

fn has_restrictor(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    low.contains("limited to")
        || low.contains("no more than")
        || terms(s).iter().any(|term| {
            matches!(
                term.as_str(),
                "only" | "solely" | "merely" | "exclusively" | "just"
            )
        })
}

fn required_poison_terms(poison: &str) -> HashSet<String> {
    let generic = HashSet::from([
        "The", "A", "An", "This", "That", "It", "Where", "When", "How", "What",
    ]);
    Regex::new(r"\b[A-Z][A-Za-z0-9_-]*\b")
        .unwrap()
        .find_iter(poison)
        .map(|m| m.as_str())
        .filter(|word| !generic.contains(word))
        .map(|word| stem(&word.to_ascii_lowercase()))
        .collect()
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        100.0
    } else {
        n as f64 / d as f64 * 100.0
    }
}

fn walk_corpus(
    root: &Path,
    allow_sensitive: bool,
    markdown_only: bool,
) -> Result<(Vec<WalkedFile>, Vec<Skip>)> {
    let mut files = Vec::new();
    let mut skipped = if allow_sensitive {
        Vec::new()
    } else {
        scan_sensitive_paths(root)?
    };
    let mut reported_sensitive: HashSet<String> = skipped.iter().map(|s| s.path.clone()).collect();
    let mut builder = corpus_walk_builder(root);
    builder.filter_entry(move |entry| {
        let path = entry.path();
        !entry
            .file_type()
            .is_some_and(|t| t.is_dir() && path_is_pruned(path))
    });
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_err) => {
                skipped.push(Skip {
                    path: String::new(),
                    reason: "walk_error".into(),
                    adapter: None,
                });
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = rel_path(root, entry.path());
        let adapter = adapter_for(entry.path());
        if markdown_only && adapter != "markdown" {
            skipped.push(Skip {
                path: rel,
                reason: "unsupported_markdown_gate".into(),
                adapter: Some(adapter),
            });
            continue;
        }
        if !allow_sensitive && is_sensitive_path(Path::new(&rel), false) {
            if reported_sensitive.insert(rel.clone()) {
                skipped.push(Skip {
                    path: rel,
                    reason: "sensitive".into(),
                    adapter: Some(adapter),
                });
            }
            continue;
        }
        if is_binary_or_unsupported(entry.path()) && !matches!(adapter.as_str(), "pdf" | "docx") {
            skipped.push(Skip {
                path: rel,
                reason: "binary_or_unsupported".into(),
                adapter: Some(adapter),
            });
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES && !matches!(adapter.as_str(), "pdf" | "docx") {
            skipped.push(Skip {
                path: rel,
                reason: "too_large".into(),
                adapter: Some(adapter),
            });
            continue;
        }
        let text = match read_adapter_text(entry.path(), &adapter) {
            Ok(t) => t,
            Err(_) if matches!(adapter.as_str(), "pdf" | "docx") => String::new(),
            Err(_) => {
                skipped.push(Skip {
                    path: rel,
                    reason: "decode_error".into(),
                    adapter: Some(adapter),
                });
                continue;
            }
        };
        if text.contains('\0') {
            skipped.push(Skip {
                path: rel,
                reason: "binary_nul".into(),
                adapter: Some(adapter),
            });
            continue;
        }
        let hash = sha256(text.as_bytes());
        files.push(WalkedFile {
            rel,
            text,
            adapter,
            hash,
        });
    }
    skipped.extend(harness_dir_skips(root));
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.dedup_by(|a, b| a.path == b.path && a.reason == b.reason);
    Ok((files, skipped))
}

fn corpus_walk_builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .add_custom_ignore_filename(".scoutignore");
    builder
}

fn path_is_pruned(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_pruned_dir)
}

fn harness_dir_skips(root: &Path) -> Vec<Skip> {
    HARNESS_DIRS
        .iter()
        .filter(|name| root.join(name).is_dir())
        .map(|name| Skip {
            path: (*name).into(),
            reason: "harness_meta".into(),
            adapter: None,
        })
        .collect()
}

fn scan_sensitive_paths(root: &Path) -> Result<Vec<Skip>> {
    fn rec(root: &Path, dir: &Path, out: &mut Vec<Skip>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = rel_path(root, &path);
            let file_type = entry.file_type()?;
            if is_sensitive_path(Path::new(&rel), file_type.is_dir()) {
                out.push(Skip {
                    path: rel,
                    reason: "sensitive".into(),
                    adapter: Some(adapter_for(&path)),
                });
                if file_type.is_dir() {
                    continue;
                }
            }
            if file_type.is_dir() {
                let name = entry.file_name();
                if is_pruned_dir(name.to_str().unwrap_or("")) {
                    continue;
                }
                rec(root, &path, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    rec(root, root, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

fn is_harness_dir(name: &str) -> bool {
    HARNESS_DIRS.contains(&name)
}

fn is_pruned_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".scout"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | "__pycache__"
            | ".venv"
            | "venv"
    ) || is_harness_dir(name)
}

fn read_adapter_text(path: &Path, adapter: &str) -> Result<String> {
    match adapter {
        "pdf" => {
            if !command_exists("pdftotext") {
                bail!("pdftotext missing");
            }
            let out = Command::new("pdftotext")
                .arg("-layout")
                .arg(path)
                .arg("-")
                .output()?;
            if !out.status.success() {
                bail!("pdftotext failed");
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        "docx" => {
            if !command_exists("pandoc") {
                bail!("pandoc missing");
            }
            let out = Command::new("pandoc")
                .arg(path)
                .arg("-t")
                .arg("markdown")
                .output()?;
            if !out.status.success() {
                bail!("pandoc failed");
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        _ => {
            let mut s = String::new();
            File::open(path)?
                .take(MAX_FILE_BYTES + 1)
                .read_to_string(&mut s)?;
            Ok(s)
        }
    }
}

fn is_binary_or_unsupported(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "zip"
            | "gz"
            | "tar"
            | "ico"
            | "woff"
            | "woff2"
            | "map"
            | "lock"
            | "jsonl"
            | "svg"
            | "mp4"
            | "mp3"
            | "heic"
    )
}

fn adapter_for(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" | "txt" | "rst" => "markdown".into(),
        "pdf" => "pdf".into(),
        "docx" => "docx".into(),
        _ => "code".into(),
    }
}

fn is_sensitive_path(rel: &Path, _is_dir: bool) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if parts.iter().any(|part| {
        matches!(
            part.as_str(),
            ".ssh" | ".aws" | ".kube" | ".docker" | ".gnupg" | ".azure"
        )
    }) {
        return true;
    }
    if parts
        .windows(2)
        .any(|pair| pair[0] == ".config" && pair[1] == "gcloud")
    {
        return true;
    }
    if parts
        .iter()
        .take(parts.len().saturating_sub(1))
        .any(|part| part.starts_with(".env"))
    {
        return true;
    }
    let name = parts.last().map(String::as_str).unwrap_or("");
    let ssh_private_key = ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        && !name.ends_with(".pub");
    if name.starts_with(".env")
        || name.ends_with(".env")
        || matches!(
            name,
            ".netrc" | ".npmrc" | ".pypirc" | ".pgpass" | ".htpasswd"
        )
        || ssh_private_key
        || name.contains("credentials")
    {
        return true;
    }
    [
        ".pem",
        ".key",
        ".p12",
        ".crt",
        ".cer",
        ".pfx",
        ".jks",
        ".keystore",
        ".ppk",
        ".asc",
        ".gpg",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn skeletonize(file: &WalkedFile, churn: u32) -> Card {
    let loc = file.text.lines().count();
    Card {
        schema_version: CARD_SCHEMA_VERSION,
        path: file.rel.clone(),
        hash: file.hash.clone(),
        adapter: file.adapter.clone(),
        symbols: symbols(&file.text, &file.adapter),
        imports: imports(&file.text),
        outline: outline(&file.text),
        churn,
        loc,
        harness_meta: is_harness_meta(&file.rel),
        role: ModelHint {
            model_hint: true,
            value: card_role(&deterministic_role(&file.rel, &file.text)),
        },
        invariants: ModelHint {
            model_hint: true,
            value: Vec::new(),
        },
        gotchas: ModelHint {
            model_hint: true,
            value: Vec::new(),
        },
        terms: ModelHint {
            model_hint: true,
            value: defined_terms(&file.text),
        },
    }
}

fn symbols(text: &str, adapter: &str) -> Vec<Symbol> {
    if adapter == "markdown" || adapter == "pdf" || adapter == "docx" {
        return outline(text)
            .into_iter()
            .map(|o| Symbol {
                name: o.text,
                kind: "heading".into(),
                line: o.line,
            })
            .collect();
    }
    let re = Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?(?:fn|def|class|struct|enum|trait|impl|function|const)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            re.captures(line).map(|c| Symbol {
                name: c.get(1).unwrap().as_str().to_string(),
                kind: line
                    .split_whitespace()
                    .next()
                    .unwrap_or("symbol")
                    .to_string(),
                line: i + 1,
            })
        })
        .take(200)
        .collect()
}

fn imports(text: &str) -> Vec<String> {
    let re = Regex::new(
        r"^\s*(?:use\s+[^;]+;|mod\s+\w+;|import\s+.+|from\s+\S+\s+import\s+.+|#include\s+.+)",
    )
    .unwrap();
    text.lines()
        .filter_map(|line| {
            if re.is_match(line) {
                Some(line.trim().chars().take(240).collect())
            } else {
                None
            }
        })
        .take(200)
        .collect()
}

fn outline(text: &str) -> Vec<OutlineItem> {
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let trimmed = line.trim_start();
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
                Some(OutlineItem {
                    text: trimmed[hashes..].trim().to_string(),
                    line: i + 1,
                    level: hashes,
                })
            } else {
                None
            }
        })
        .take(200)
        .collect()
}

fn defined_terms(text: &str) -> Vec<String> {
    let re = Regex::new(r"(?m)^\s*(?:\*\*)?([A-Z][A-Za-z0-9 _/-]{2,40})(?:\*\*)?\s*[:—-]").unwrap();
    let mut seen = HashSet::new();
    re.captures_iter(text)
        .filter_map(|c| {
            let term = c.get(1).unwrap().as_str().trim().to_string();
            if seen.insert(term.clone()) {
                Some(term)
            } else {
                None
            }
        })
        .take(50)
        .collect()
}

fn is_harness_meta(rel: &str) -> bool {
    let name = Path::new(rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    rel == "CLAUDE.md"
        || rel == "AGENTS.md"
        || rel.contains("/CLAUDE.md")
        || rel.contains("/AGENTS.md")
        || rel.starts_with("memory/")
        || rel.starts_with(".claude/")
        || is_meta_file_name(name)
}

fn is_meta_file_name(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "LICENSE"
                | "LICENSE.md"
                | "COPYING"
                | "Cargo.lock"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "poetry.lock"
                | "Pipfile.lock"
                | "Gemfile.lock"
                | "go.sum"
        )
}

fn deterministic_role(path: &str, text: &str) -> String {
    if path.ends_with("CLAUDE.md") || path.ends_with("AGENTS.md") {
        return "agent working agreements and repo instructions".into();
    }
    if path.ends_with("README.md") {
        return "project overview and usage documentation".into();
    }
    if let Some(first_heading) = outline(text).first() {
        return first_heading.text.chars().take(120).collect();
    }
    let syms = symbols(text, &adapter_for(Path::new(path)));
    if let Some(sym) = syms.first() {
        return format!("defines {} and related {} code", sym.name, sym.kind);
    }
    format!("{} file", adapter_for(Path::new(path)))
}

fn card_role(raw: &str) -> String {
    redact_outbound(raw.trim()).0.chars().take(160).collect()
}

fn git_churn(root: &Path) -> HashMap<String, u32> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--since=90 days ago", "--format=", "--name-only"])
        .output();
    let mut map = HashMap::new();
    if let Ok(out) = out
        && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            *map.entry(line.trim().to_string()).or_insert(0) += 1;
        }
    }
    map
}

const INDEX_LOCK_STALE_MS: u128 = 15 * 60 * 1000;
const UNPARSEABLE_LOCK_STALE_MS: u128 = 10 * 1000;

#[derive(Debug, Serialize, Deserialize)]
struct IndexLockMeta {
    pid: u32,
    started_at_ms: u128,
}

#[derive(Debug)]
struct IndexLockDetail {
    meta: Option<IndexLockMeta>,
    file_age_ms: Option<u128>,
}

struct IndexLock {
    path: PathBuf,
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn write_generation(root: &Path, manifest: &Manifest, cards: &[Card]) -> Result<PathBuf> {
    let scout = root.join(".scout");
    fs::create_dir_all(&scout)?;
    let lock_path = scout.join("lock");
    let _lock = acquire_index_lock(&lock_path)?;
    (|| -> Result<PathBuf> {
        let mut nonce = unix_ms();
        let (gen_name, gen_dir) = loop {
            let name = format!("gen-{nonce}");
            let dir = scout.join(&name);
            if !dir.exists() {
                fs::create_dir(&dir)?;
                break (name, dir);
            }
            nonce += 1;
        };
        let mut cards_file = File::create(gen_dir.join("cards.jsonl"))?;
        for card in cards {
            writeln!(cards_file, "{}", serde_json::to_string(card)?)?;
        }
        fs::write(
            gen_dir.join("manifest.json"),
            serde_json::to_vec_pretty(manifest)?,
        )?;
        let tmp = scout.join("current.tmp");
        fs::write(&tmp, &gen_name)?;
        fs::rename(tmp, scout.join("current"))?;
        Ok(gen_dir)
    })()
}

fn acquire_index_lock(lock_path: &Path) -> Result<IndexLock> {
    match create_index_lock(lock_path) {
        Ok(()) => Ok(IndexLock {
            path: lock_path.to_path_buf(),
        }),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let detail = lock_detail(lock_path);
            if lock_is_stale(&detail) {
                let _ = fs::remove_file(lock_path);
                create_index_lock(lock_path)
                    .with_context(|| lock_exists_message(&lock_detail(lock_path)))?;
                return Ok(IndexLock {
                    path: lock_path.to_path_buf(),
                });
            }
            bail!("{}", lock_exists_message(&detail));
        }
        Err(err) => Err(err).context("failed to create scout index lock"),
    }
}

fn create_index_lock(lock_path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)?;
    let body = serde_json::to_vec(&IndexLockMeta {
        pid: std::process::id(),
        started_at_ms: unix_ms(),
    })
    .map_err(io::Error::other)?;
    if let Err(err) = file.write_all(&body).and_then(|_| file.flush()) {
        let _ = fs::remove_file(lock_path);
        return Err(err);
    }
    Ok(())
}

fn lock_detail(lock_path: &Path) -> IndexLockDetail {
    let file_age_ms = fs::metadata(lock_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_millis());
    let meta = fs::read_to_string(lock_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    IndexLockDetail { meta, file_age_ms }
}

fn lock_is_stale(detail: &IndexLockDetail) -> bool {
    lock_is_stale_at(detail, unix_ms(), pid_alive)
}

fn lock_is_stale_at(
    detail: &IndexLockDetail,
    now_ms: u128,
    pid_alive: impl Fn(u32) -> bool,
) -> bool {
    let Some(meta) = &detail.meta else {
        return detail
            .file_age_ms
            .is_some_and(|age| age > UNPARSEABLE_LOCK_STALE_MS);
    };
    meta.pid == std::process::id()
        || now_ms.saturating_sub(meta.started_at_ms) > INDEX_LOCK_STALE_MS
        || !pid_alive(meta.pid)
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid = pid.to_string();
    Command::new("ps")
        .arg("-p")
        .arg(pid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn lock_exists_message(detail: &IndexLockDetail) -> String {
    match &detail.meta {
        Some(meta) => format!(
            "another scout index appears to be running (.scout/lock exists; pid={}, age_ms={})",
            meta.pid,
            unix_ms().saturating_sub(meta.started_at_ms)
        ),
        None => {
            "another scout index appears to be running (.scout/lock exists; pid=unknown, age_ms=unknown)"
                .into()
        }
    }
}

fn open_snapshot(root: &Path) -> Result<Snapshot> {
    let scout = root.join(".scout");
    let gen_name = fs::read_to_string(scout.join("current"))?
        .trim()
        .to_string();
    let gen_dir = scout.join(&gen_name);
    let manifest: Manifest =
        serde_json::from_str(&fs::read_to_string(gen_dir.join("manifest.json"))?)?;
    Ok(Snapshot { gen_dir, manifest })
}

fn load_cards(snapshot: &Snapshot) -> Result<Vec<Card>> {
    let raw = fs::read_to_string(snapshot.gen_dir.join("cards.jsonl"))?;
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn read_card_files(root: &Path, cards: &[Card]) -> HashMap<String, String> {
    cards
        .iter()
        .filter_map(|c| {
            let path = root.join(&c.path);
            read_adapter_text(&path, &c.adapter)
                .ok()
                .map(|text| (c.path.clone(), text))
        })
        .collect()
}

fn manifest_file_meta(root: &Path, files: &[WalkedFile]) -> BTreeMap<String, ManifestFileMeta> {
    files
        .iter()
        .filter_map(|f| file_meta(root, &f.rel).map(|meta| (f.rel.clone(), meta)))
        .collect()
}

fn file_meta(root: &Path, rel: &str) -> Option<ManifestFileMeta> {
    let meta = fs::metadata(root.join(rel)).ok()?;
    Some(ManifestFileMeta {
        size: meta.len(),
        mtime_unix_ms: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or_default(),
    })
}

fn walk_corpus_file_meta(
    root: &Path,
    markdown_only: bool,
) -> Result<BTreeMap<String, ManifestFileMeta>> {
    let mut out = BTreeMap::new();
    let mut builder = corpus_walk_builder(root);
    builder.filter_entry(|entry| {
        let path = entry.path();
        !entry
            .file_type()
            .is_some_and(|t| t.is_dir() && path_is_pruned(path))
    });
    for entry in builder.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = rel_path(root, entry.path());
        let adapter = adapter_for(entry.path());
        if markdown_only && adapter != "markdown" {
            continue;
        }
        if is_sensitive_path(Path::new(&rel), false)
            || (is_binary_or_unsupported(entry.path())
                && !matches!(adapter.as_str(), "pdf" | "docx"))
        {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.len() > MAX_FILE_BYTES && !matches!(adapter.as_str(), "pdf" | "docx") {
            continue;
        }
        out.insert(
            rel,
            ManifestFileMeta {
                size: meta.len(),
                mtime_unix_ms: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or_default(),
            },
        );
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct StalenessReport {
    stale: bool,
    reason: String,
    changed_files: usize,
}

fn staleness_report(root: &Path, manifest: &Manifest, cards: &[Card]) -> Result<StalenessReport> {
    if !manifest_current(manifest, root) {
        Ok(StalenessReport {
            stale: true,
            reason: "manifest_mismatch".into(),
            changed_files: 0,
        })
    } else {
        let changed_files = changed_files_since_cards(root, manifest, cards)?;
        Ok(if changed_files > 0 {
            StalenessReport {
                stale: true,
                reason: "content_hash_mismatch".into(),
                changed_files,
            }
        } else {
            StalenessReport {
                stale: false,
                reason: "current".into(),
                changed_files: 0,
            }
        })
    }
}

fn changed_files_since_cards(root: &Path, manifest: &Manifest, cards: &[Card]) -> Result<usize> {
    let current_meta = walk_corpus_file_meta(root, manifest.markdown_only)?;
    let indexed: HashMap<String, &Card> = cards.iter().map(|c| (c.path.clone(), c)).collect();
    let paths: HashSet<String> = current_meta.keys().chain(indexed.keys()).cloned().collect();
    let mut changed = 0;
    for path in paths {
        let (Some(current), Some(card)) = (current_meta.get(&path), indexed.get(&path)) else {
            changed += 1;
            continue;
        };
        if manifest.file_meta.get(&path) == Some(current) {
            continue;
        }
        let text = read_adapter_text(&root.join(&path), &card.adapter).unwrap_or_default();
        if sha256(text.as_bytes()) != card.hash {
            changed += 1;
        }
    }
    Ok(changed)
}

fn manifest_current(m: &Manifest, root: &Path) -> bool {
    m.schema_version == CARD_SCHEMA_VERSION
        && m.card_prompt_hash == sha256(CARD_PROMPT.as_bytes())
        && m.model == env::var("SCOUT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
        && m.adapter_version == adapter_version()
        && m.ignore_config_hash == ignore_config_hash(root)
}

fn manifest_compatible(m: &Manifest, model: &str, root: &Path) -> bool {
    m.schema_version == CARD_SCHEMA_VERSION
        && m.card_prompt_hash == sha256(CARD_PROMPT.as_bytes())
        && m.model == model
        && m.adapter_version == adapter_version()
        && m.ignore_config_hash == ignore_config_hash(root)
}

fn prune_generations(root: &Path, keep: usize) -> Result<()> {
    let scout = root.join(".scout");
    let current = fs::read_to_string(scout.join("current"))
        .ok()
        .map(|s| scout.join(s.trim()));
    let mut generations = Vec::new();
    for entry in fs::read_dir(&scout)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("gen-") && entry.file_type()?.is_dir() {
            generations.push((generation_timestamp(&name), entry.path()));
        }
    }
    generations.sort_by_key(|g| std::cmp::Reverse(g.0));
    for (_, path) in generations.into_iter().skip(keep) {
        if current.as_ref().is_some_and(|c| *c == path) {
            continue;
        }
        let _ = fs::remove_dir_all(path);
    }
    Ok(())
}

fn generation_timestamp(name: &str) -> u128 {
    name.strip_prefix("gen-")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn route_with_model(
    client: &CerebrasClient,
    gate: &BudgetGate,
    query: &str,
    cards: &[Card],
) -> Result<Vec<Candidate>> {
    let input = thin_cards_json(query, cards);
    if !gate.may_launch(&client.spend, estimate_chat_cost(&input, 4000)) {
        return Err(budget_hit_error("budget cap hit before router"));
    }
    let raw = client.chat_with_budget(
        vec![
            Message::system("You route scout extraction. Return ONLY JSON {\"files\":[{\"path\":\"...\",\"look_for\":\"...\",\"rank\":1}]}. Use only paths from input."),
            Message::user(input),
        ],
        4000,
        Some(gate),
    )?;
    #[derive(Deserialize)]
    struct RouterOut {
        files: Vec<RouterFile>,
    }
    #[derive(Deserialize)]
    struct RouterFile {
        path: String,
        #[serde(default)]
        look_for: String,
        #[serde(default)]
        rank: usize,
    }
    let valid: HashSet<&str> = cards.iter().map(|c| c.path.as_str()).collect();
    let out: RouterOut = parse_jsonish(&raw)?;
    Ok(out
        .files
        .into_iter()
        .filter(|f| valid.contains(f.path.as_str()))
        .map(|f| Candidate {
            path: f.path,
            router_rank: Some(if f.rank == 0 { 999 } else { f.rank }),
            deterministic_score: 0.0,
            look_for: Some(f.look_for),
        })
        .collect())
}

fn thin_cards_json(query: &str, cards: &[Card]) -> String {
    serde_json::to_string(&json!({
        "query": query,
        "cards": cards.iter().map(|c| json!({"path": c.path, "role": card_role(&c.role.value)})).collect::<Vec<_>>()
    })).unwrap()
}

fn deterministic_candidates(
    query: &str,
    cards: &[Card],
    files: &HashMap<String, String>,
    limit: usize,
) -> Vec<Candidate> {
    let qterms = terms(query);
    if qterms.is_empty() {
        return Vec::new();
    }
    let mut df: HashMap<String, usize> = HashMap::new();
    let file_terms: HashMap<String, HashSet<String>> = cards
        .iter()
        .map(|c| {
            let text = format!(
                "{} {} {} {}",
                c.path,
                c.role.value,
                c.symbols
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                files
                    .get(&c.path)
                    .map(|s| truncate_chars(s, 20_000))
                    .unwrap_or_default()
            );
            let set: HashSet<String> = terms(&text).into_iter().collect();
            for t in &qterms {
                if set.contains(t) {
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
            }
            (c.path.clone(), set)
        })
        .collect();
    let mut scored = Vec::new();
    for c in cards {
        let set = file_terms.get(&c.path).unwrap();
        let mut score = 0.0;
        for t in &qterms {
            if set.contains(t) {
                score += 1.0 / (*df.get(t).unwrap_or(&1) as f64);
            }
        }
        if c.path
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
        {
            score += 2.0;
        }
        if score > 0.0 {
            scored.push(Candidate {
                path: c.path.clone(),
                router_rank: None,
                deterministic_score: score,
                look_for: None,
            });
        }
    }
    scored.sort_by(|a, b| {
        b.deterministic_score
            .partial_cmp(&a.deterministic_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    scored.truncate(limit);
    scored
}

fn final_candidates(
    router: Vec<Candidate>,
    deterministic: Vec<Candidate>,
    cards: &[Card],
) -> Vec<Candidate> {
    let valid: HashSet<String> = cards.iter().map(|c| c.path.clone()).collect();
    let mut by_path: BTreeMap<String, Candidate> = BTreeMap::new();
    for c in router.into_iter().chain(deterministic) {
        if !valid.contains(&c.path) {
            continue;
        }
        by_path
            .entry(c.path.clone())
            .and_modify(|old| {
                old.router_rank = old.router_rank.or(c.router_rank);
                old.deterministic_score = old.deterministic_score.max(c.deterministic_score);
                if old.look_for.as_ref().is_none_or(|s| s.is_empty()) {
                    old.look_for = c.look_for.clone();
                }
            })
            .or_insert(c);
    }
    let mut out: Vec<Candidate> = by_path.into_values().collect();
    out.sort_by(|a, b| {
        a.router_rank
            .unwrap_or(usize::MAX)
            .cmp(&b.router_rank.unwrap_or(usize::MAX))
            .then_with(|| {
                b.deterministic_score
                    .partial_cmp(&a.deterministic_score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn top_candidate_deterministic_score(candidates: &[Candidate]) -> f64 {
    candidates
        .iter()
        .map(|c| c.deterministic_score)
        .fold(0.0, f64::max)
}

fn build_chunks(
    candidates: &[Candidate],
    files: &HashMap<String, String>,
    mode: &str,
    query: &str,
) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let chunk_mode = if mode == "ranked_boundary" {
        "boundary_overlap"
    } else {
        mode
    };
    for c in candidates {
        if let Some(text) = files.get(&c.path) {
            for (first_line, body) in chunks_for(&c.path, text, chunk_mode) {
                chunks.push(Chunk {
                    file: c.path.clone(),
                    first_line,
                    body,
                    router_rank: c.router_rank,
                    deterministic_score: c.deterministic_score,
                    look_for: c.look_for.clone(),
                });
            }
        }
    }
    if mode == "ranked_boundary" {
        rank_chunks(
            chunks,
            query,
            env::var("SCOUT_CHUNK_CAP")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(24),
        )
    } else {
        chunks
    }
}

fn rank_chunks(mut chunks: Vec<Chunk>, query: &str, cap: usize) -> Vec<Chunk> {
    if chunks.len() <= cap {
        return chunks;
    }
    let qterms = terms(query);
    let mut best_by_file: BTreeMap<String, (f64, Chunk)> = BTreeMap::new();
    for chunk in &chunks {
        let score = chunk_score(chunk, &qterms);
        best_by_file
            .entry(chunk.file.clone())
            .and_modify(|(old_score, old)| {
                if score > *old_score {
                    *old_score = score;
                    *old = chunk.clone();
                }
            })
            .or_insert((score, chunk.clone()));
    }
    chunks.sort_by(|a, b| {
        chunk_score(b, &qterms)
            .partial_cmp(&chunk_score(a, &qterms))
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.first_line.cmp(&b.first_line))
    });
    let mut best = best_by_file.into_values().collect::<Vec<_>>();
    best.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    best.truncate(cap);
    let mut out = best.into_iter().map(|(_, chunk)| chunk).collect::<Vec<_>>();
    let mut seen: HashSet<(String, usize)> = out
        .iter()
        .map(|chunk| (chunk.file.clone(), chunk.first_line))
        .collect();
    for chunk in chunks {
        if out.len() >= cap {
            break;
        }
        if seen.insert((chunk.file.clone(), chunk.first_line)) {
            out.push(chunk);
        }
    }
    out.sort_by(|a, b| {
        a.router_rank
            .unwrap_or(usize::MAX)
            .cmp(&b.router_rank.unwrap_or(usize::MAX))
            .then_with(|| {
                b.deterministic_score
                    .partial_cmp(&a.deterministic_score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.first_line.cmp(&b.first_line))
    });
    out
}

fn chunk_score(chunk: &Chunk, qterms: &[String]) -> f64 {
    let low = chunk.body.to_ascii_lowercase();
    let look_terms = terms(chunk.look_for.as_deref().unwrap_or(""));
    let mut score = chunk.deterministic_score;
    for term in qterms.iter().chain(look_terms.iter()) {
        let count = low.matches(term).count();
        if count > 0 {
            score += 2.0 + count as f64;
        }
    }
    if chunk.router_rank.is_some_and(|rank| rank <= 3) {
        score += 5.0;
    }
    score
}

fn chunks_for(path: &str, text: &str, mode: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    if mode != "fixed" {
        let adapter = adapter_for(Path::new(path));
        if adapter == "markdown" || adapter == "pdf" || adapter == "docx" {
            for o in outline(text) {
                if o.line > 1 {
                    starts.push(o.line - 1);
                }
            }
        } else {
            for s in symbols(text, "code") {
                if s.line > 1 {
                    starts.push(s.line - 1);
                }
            }
        }
        starts.sort_unstable();
        starts.dedup();
    } else {
        starts = (0..lines.len()).step_by(350).collect();
    }
    let mut out = Vec::new();
    for (idx, start) in starts.iter().enumerate() {
        let mut end = starts
            .get(idx + 1)
            .copied()
            .unwrap_or(lines.len())
            .min(start + 350);
        if mode == "boundary_overlap" {
            end = (end + 25).min(lines.len());
        }
        if end <= *start {
            continue;
        }
        out.push((*start + 1, numbered_lines(&lines[*start..end], *start + 1)));
    }
    if out.is_empty() {
        out.push((1, numbered_lines(&lines[..lines.len().min(350)], 1)));
    }
    out
}

fn numbered_lines(lines: &[&str], first_line: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}| {}", i + first_line, l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn verify_quotes(
    findings: Vec<Finding>,
    files: &HashMap<String, String>,
) -> (Vec<Finding>, Vec<DroppedFinding>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for mut f in findings {
        let Some(text) = files.get(&f.file) else {
            dropped.push(drop_reason(&f, "file_missing"));
            continue;
        };
        let q = norm(f.quote.as_deref().unwrap_or(""));
        if q.is_empty() || f.fact.trim().is_empty() {
            dropped.push(drop_reason(&f, "empty"));
            continue;
        }
        let normalized_lines: Vec<String> = text.lines().map(norm).collect();
        match quote_match(&normalized_lines, &q, f.line) {
            QuoteMatch::Matched => {
                f.match_tier = Some("exact".into());
                kept.push(f);
            }
            exact_miss => {
                if adapter_for(Path::new(&f.file)) != "markdown" {
                    match exact_miss {
                        QuoteMatch::LineMismatch(actual) => {
                            dropped.push(drop_reason(&f, &format!("line_mismatch:{actual}")));
                        }
                        _ => dropped.push(drop_reason(&f, "quote_not_in_file")),
                    }
                    continue;
                }
                let markdown_lines: Vec<String> = normalized_lines
                    .iter()
                    .map(|line| strip_markdown_emphasis(line))
                    .collect();
                let markdown_quote = strip_markdown_emphasis(&q);
                match quote_match(&markdown_lines, &markdown_quote, f.line) {
                    QuoteMatch::Matched => {
                        f.match_tier = Some("markdown_normalized".into());
                        kept.push(f);
                    }
                    QuoteMatch::LineMismatch(actual) => {
                        dropped.push(drop_reason(&f, &format!("line_mismatch:{actual}")));
                    }
                    QuoteMatch::NotFound => match exact_miss {
                        QuoteMatch::LineMismatch(actual) => {
                            dropped.push(drop_reason(&f, &format!("line_mismatch:{actual}")));
                        }
                        _ => dropped.push(drop_reason(&f, "quote_not_in_file")),
                    },
                }
            }
        }
    }
    (kept, dropped)
}

enum QuoteMatch {
    Matched,
    LineMismatch(usize),
    NotFound,
}

fn quote_match(lines: &[String], quote: &str, cited_line: usize) -> QuoteMatch {
    if quote.is_empty() {
        return QuoteMatch::NotFound;
    }
    let full = lines.join(" ");
    let Some(idx) = full.find(quote) else {
        return QuoteMatch::NotFound;
    };
    let mut offset = 0usize;
    for (line_idx, line) in lines.iter().enumerate() {
        if idx < offset + line.len() + 1 {
            let actual = line_idx + 1;
            return if cited_line.abs_diff(actual) <= 5 {
                QuoteMatch::Matched
            } else {
                QuoteMatch::LineMismatch(actual)
            };
        }
        offset += line.len() + 1;
    }
    QuoteMatch::NotFound
}

fn strip_markdown_emphasis(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect()
}

fn matches_exact_claim_query(query: &str, finding: &Finding) -> bool {
    let Some(required) = exact_claim_terms(query) else {
        return true;
    };
    let found: HashSet<String> = terms(finding.quote.as_deref().unwrap_or(""))
        .into_iter()
        .collect();
    required.is_subset(&found)
}

fn exact_claim_terms(query: &str) -> Option<HashSet<String>> {
    let trimmed = query.trim().trim_end_matches(['?', '.', '!']).trim();
    let low = trimmed.to_ascii_lowercase();
    let prefix = "where does the corpus say ";
    if !low.starts_with(prefix) {
        return None;
    }
    let relation_terms = HashSet::from(["claim", "cover", "creat", "describe", "include", "say"]);
    let required = terms(&trimmed[prefix.len()..])
        .into_iter()
        .filter(|term| !relation_terms.contains(term.as_str()))
        .collect::<HashSet<_>>();
    (required.len() >= 3).then_some(required)
}

fn drop_reason(f: &Finding, reason: &str) -> DroppedFinding {
    DroppedFinding {
        file: f.file.clone(),
        line: Some(f.line),
        fact: Some(f.fact.clone()),
        quote: f.quote.clone(),
        reason: reason.into(),
    }
}

fn sort_findings(a: &Finding, b: &Finding) -> Ordering {
    a.router_rank
        .unwrap_or(usize::MAX)
        .cmp(&b.router_rank.unwrap_or(usize::MAX))
        .then_with(|| {
            b.deterministic_score
                .partial_cmp(&a.deterministic_score)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| a.file.cmp(&b.file))
        .then_with(|| a.line.cmp(&b.line))
}

fn is_weak_signal(findings: &[Finding], top_candidate_score: f64) -> bool {
    !findings.is_empty()
        && findings
            .iter()
            .map(|f| f.deterministic_score)
            .fold(0.0, f64::max)
            < top_candidate_score * 0.4
}

fn persist_query_artifacts(root: &Path, env: &mut Envelope) -> Result<()> {
    let scout = root.join(".scout");
    fs::create_dir_all(&scout)?;
    let runs = scout.join("runs");
    fs::create_dir_all(&runs)?;
    let mut stamp = unix_ms();
    let run_path = loop {
        let path = runs.join(format!("{stamp}.json"));
        if !path.exists() {
            break path;
        }
        stamp += 1;
    };
    let last_path = scout.join("last-run.json");
    insert_data_field(
        &mut env.data,
        "last_run_path",
        json!(last_path.display().to_string()),
    );
    insert_data_field(
        &mut env.data,
        "run_path",
        json!(run_path.display().to_string()),
    );
    let body = serde_json::to_vec_pretty(env)?;
    atomic_write(&last_path, &body)?;
    atomic_write(&run_path, &body)?;
    prune_query_runs(&runs, 20)?;
    Ok(())
}

fn query_stdout_envelope(env: &Envelope, budget_tokens: Option<usize>, compact: bool) -> Envelope {
    let mut out = env.clone();
    let findings = serde_json::from_value::<Vec<Finding>>(
        out.data
            .get("findings")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .unwrap_or_default();
    let visible_findings = pack_findings(findings, budget_tokens);
    out.data["findings"] = if compact {
        json!(compact_findings(&visible_findings))
    } else {
        json!(visible_findings)
    };
    if compact {
        let dropped = serde_json::from_value::<Vec<DroppedFinding>>(
            out.data
                .get("dropped")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .unwrap_or_default();
        out.data["dropped"] = json!(dropped_reason_counts(&dropped));
    }
    out
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", unix_ms()));
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn prune_query_runs(runs: &Path, keep: usize) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(runs)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext == "json")
        {
            files.push(entry.path());
        }
    }
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    for path in files.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn insert_data_field(data: &mut Value, key: &str, value: Value) {
    if let Value::Object(map) = data {
        map.insert(key.into(), value);
    }
}

fn compact_findings(findings: &[Finding]) -> Vec<Value> {
    findings
        .iter()
        .map(|f| {
            let mut value = json!({
                "file": &f.file,
                "line": f.line,
                "fact": &f.fact,
                "quote": &f.quote,
                "quote_omitted": f.quote_omitted,
                "match_tier": &f.match_tier,
            });
            if f.quote.is_none() {
                value.as_object_mut().unwrap().remove("quote");
            }
            if f.match_tier.is_none() {
                value.as_object_mut().unwrap().remove("match_tier");
            }
            value
        })
        .collect()
}

fn dropped_reason_counts(dropped: &[DroppedFinding]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for d in dropped {
        *counts.entry(d.reason.clone()).or_insert(0) += 1;
    }
    counts
}

fn pack_findings(mut findings: Vec<Finding>, budget_tokens: Option<usize>) -> Vec<Finding> {
    let Some(limit) = budget_tokens else {
        return findings;
    };
    let mut used = 200usize;
    for f in &mut findings {
        let quoted = serde_json::to_string(f).unwrap_or_default();
        let cost = estimate_tokens(&quoted);
        if used + cost <= limit {
            used += cost;
        } else {
            f.quote = None;
            f.quote_omitted = true;
            used += estimate_tokens(&serde_json::to_string(f).unwrap_or_default());
        }
    }
    findings
}

fn redact_outbound(text: &str) -> (String, usize) {
    let mut out = Regex::new(r"/(?:Users|home)/[A-Za-z0-9._-]+/")
        .unwrap()
        .replace_all(text, "~/")
        .into_owned();
    let mut count = 0;
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut placeholder = |kind: &str, literal: &str| -> String {
        if let Some(v) = seen.get(literal) {
            return v.clone();
        }
        let n = seen.len() + 1;
        let ph = if kind == "EMAIL" {
            format!("[REDACTED_EMAIL_{n}]")
        } else {
            format!("[REDACTED_SECRET_{n} len={}]", literal.len())
        };
        seen.insert(literal.to_string(), ph.clone());
        ph
    };

    let bearer = Regex::new(r"\b(Bearer\s+)([A-Za-z0-9._~+/-]{12,}=*)").unwrap();
    out = bearer
        .replace_all(&out, |caps: &regex::Captures| {
            count += 1;
            format!("{}{}", &caps[1], placeholder("SECRET", &caps[2]))
        })
        .into_owned();

    let keyish = r"[A-Za-z0-9_.-]*(?:key|token|secret|password|api|credential|auth)[A-Za-z0-9_.-]*";
    for pat in [
        format!(r#"(?i)(["'][^"'\n]*{keyish}[^"'\n]*["']\s*:\s*["'])([^"'\n]+)(["'])"#),
        format!(r#"(?i)(\b{keyish}\s*=\s*["']?)([^"'\s#;]+)"#),
        r#"(?i)(--[A-Za-z0-9_-]*(?:key|token|secret|password|api|credential|auth)[A-Za-z0-9_-]*\s+)([^\s]+)"#.to_string(),
        format!(r#"(?im)(^\s*["']?{keyish}["']?\s*:\s*["']?)([^\n"',}}]+)"#),
    ] {
        let re = Regex::new(&pat).unwrap();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                let value = caps.get(2).unwrap().as_str().trim();
                if skip_secret(value) {
                    caps.get(0).unwrap().as_str().to_string()
                } else {
                    count += 1;
                    format!(
                        "{}{}{}",
                        caps.get(1).unwrap().as_str(),
                        placeholder("SECRET", value),
                        caps.get(3).map(|m| m.as_str()).unwrap_or("")
                    )
                }
            })
            .into_owned();
    }

    for pat in [
        r"\beyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",
        r"(?:sk-ant-[A-Za-z0-9._-]{8,}|sk-[A-Za-z0-9._-]{8,}|csk-[A-Za-z0-9._-]{8,}|AKIA[0-9A-Z]{12,20}|ghp_[A-Za-z0-9_]{8,}|github_pat_[A-Za-z0-9_]{20,}|gho_[A-Za-z0-9_]{8,}|ghu_[A-Za-z0-9_]{8,}|glpat-[A-Za-z0-9_-]{8,}|npm_[A-Za-z0-9_-]{8,}|pypi-[A-Za-z0-9_-]{8,}|xox[bpars]-[A-Za-z0-9-]{8,})",
    ] {
        let re = Regex::new(pat).unwrap();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                let v = caps.get(0).unwrap().as_str();
                if skip_secret(v) {
                    v.to_string()
                } else {
                    count += 1;
                    placeholder("SECRET", v)
                }
            })
            .into_owned();
    }

    let entropy = Regex::new(r"([A-Za-z0-9+/=_-]{32,})").unwrap();
    out = entropy
        .replace_all(&out, |caps: &regex::Captures| {
            let v = caps.get(1).unwrap().as_str();
            if entropy_secret(v) {
                count += 1;
                placeholder("SECRET", v)
            } else {
                v.to_string()
            }
        })
        .into_owned();

    let email = Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap();
    out = email
        .replace_all(&out, |caps: &regex::Captures| {
            count += 1;
            placeholder("EMAIL", caps.get(0).unwrap().as_str())
        })
        .into_owned();
    (out, count)
}

fn skip_secret(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.contains("[REDACTED_") {
        return true;
    }
    Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
        .unwrap()
        .is_match(value)
        || (value.len() == 40 && Regex::new(r"^[0-9a-fA-F]{40}$").unwrap().is_match(value))
}

fn entropy_secret(value: &str) -> bool {
    if skip_secret(value) || value.len() < 32 || value.matches('/').count() >= 2 {
        return false;
    }
    let distinct = value
        .trim_end_matches('=')
        .chars()
        .collect::<HashSet<_>>()
        .len();
    distinct > 20
        || (Regex::new(r"[a-z]").unwrap().is_match(value)
            && Regex::new(r"[A-Z]").unwrap().is_match(value)
            && Regex::new(r"\d").unwrap().is_match(value))
}

fn parse_jsonish<T: for<'de> Deserialize<'de>>(raw: &str) -> Result<T> {
    let mut s = raw.trim().to_string();
    if s.starts_with("```") {
        s = Regex::new(r"^```[a-zA-Z]*\n?")
            .unwrap()
            .replace(&s, "")
            .to_string();
        s = Regex::new(r"\n?```$")
            .unwrap()
            .replace(s.trim(), "")
            .to_string();
    }
    s = Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f]")
        .unwrap()
        .replace_all(&s, "")
        .to_string();
    Ok(serde_json::from_str(&s)?)
}

fn norm(s: &str) -> String {
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(s, " ")
        .trim()
        .to_string()
}

fn terms(s: &str) -> Vec<String> {
    let stop: HashSet<&str> = "the a an and or of to in for on at is are was be with how does do did what where when which who why by from it its this that as before after not any one all between during into out over under".split_whitespace().collect();
    let re = Regex::new(r"[a-z0-9_]{3,}").unwrap();
    let mut seen = HashSet::new();
    re.find_iter(&s.to_ascii_lowercase())
        .filter_map(|m| {
            let w = m.as_str();
            if stop.contains(w) {
                return None;
            }
            let stem = stem(w);
            if seen.insert(stem.clone()) {
                Some(stem)
            } else {
                None
            }
        })
        .collect()
}

fn stem(w: &str) -> String {
    for suf in ["ing", "ed", "es", "s"] {
        if w.ends_with(suf) && w.len() - suf.len() >= 3 {
            return w[..w.len() - suf.len()].to_string();
        }
    }
    w.to_string()
}

fn estimate_tokens(s: &str) -> usize {
    (s.chars().count() / 4).max(1)
}

#[derive(Debug)]
struct IndexEstimate {
    usd: f64,
    files_to_card: usize,
}

fn incremental_index_estimate(root: &Path, markdown_only: bool) -> Result<IndexEstimate> {
    let (files, _) = walk_corpus(root, false, markdown_only)?;
    let model = env::var("SCOUT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let old = open_snapshot(root)
        .ok()
        .and_then(|s| load_cards(&s).ok().map(|c| (s.manifest, c)));
    let can_reuse = old
        .as_ref()
        .is_some_and(|(m, _)| manifest_compatible(m, &model, root));
    let old_by_key: HashMap<(String, String), Card> = old
        .as_ref()
        .map(|(_, cards)| {
            cards
                .iter()
                .map(|c| ((c.path.clone(), c.hash.clone()), c.clone()))
                .collect()
        })
        .unwrap_or_default();
    let to_card = files_to_card(&files, can_reuse, &old_by_key);
    Ok(IndexEstimate {
        usd: estimate_index_cost_refs(&to_card),
        files_to_card: to_card.len(),
    })
}

fn files_to_card<'a>(
    files: &'a [WalkedFile],
    can_reuse: bool,
    old_by_key: &HashMap<(String, String), Card>,
) -> Vec<&'a WalkedFile> {
    files
        .iter()
        .filter(|f| !matches!(f.adapter.as_str(), "pdf" | "docx") || !f.text.is_empty())
        .filter(|f| !can_reuse || !old_by_key.contains_key(&(f.rel.clone(), f.hash.clone())))
        .collect()
}

fn estimate_index_cost_refs(files: &[&WalkedFile]) -> f64 {
    let toks: usize = files.iter().map(|f| estimate_tokens(&f.text) + 700).sum();
    toks as f64 / 1_000_000.0 * INPUT_DOLLARS_PER_MTOK + files.len() as f64 * 0.001
}

fn estimate_index_cost(files: &[WalkedFile]) -> f64 {
    let toks: usize = files
        .iter()
        .filter(|f| !matches!(f.adapter.as_str(), "pdf" | "docx") || !f.text.is_empty())
        .map(|f| estimate_tokens(&f.text) + 700)
        .sum();
    toks as f64 / 1_000_000.0 * INPUT_DOLLARS_PER_MTOK + files.len() as f64 * 0.001
}

fn estimate_chunks_cost(chunks: &[Chunk]) -> f64 {
    let toks: usize = chunks.iter().map(|c| estimate_tokens(&c.body) + 700).sum();
    toks as f64 / 1_000_000.0 * INPUT_DOLLARS_PER_MTOK + chunks.len() as f64 * 0.006
}

fn estimate_chat_cost(input: &str, max_output_tokens: u64) -> f64 {
    estimate_tokens(input) as f64 / 1_000_000.0 * INPUT_DOLLARS_PER_MTOK
        + max_output_tokens as f64 / 1_000_000.0 * OUTPUT_DOLLARS_PER_MTOK
}

fn parse_budget(s: &str) -> Result<usize> {
    let low = s.to_ascii_lowercase();
    if let Some(n) = low.strip_suffix('k') {
        Ok(n.parse::<usize>()
            .map_err(|err| usage_error(format!("invalid --budget: {err}")))?
            * 1000)
    } else {
        low.parse()
            .map_err(|err| usage_error(format!("invalid --budget: {err}")))
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn truncate_json_value(value: Value, tokens: usize) -> Value {
    let s = serde_json::to_string(&value).unwrap_or_default();
    if estimate_tokens(&s) <= tokens {
        value
    } else {
        json!({"truncated": true, "preview": truncate_chars(&s, tokens * 4)})
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn adapter_version() -> String {
    "regex-v3+harness-prune+file-meta+pdftotext+pandoc".into()
}

fn ignore_config_hash(root: &Path) -> String {
    let mut data = String::new();
    for name in [".gitignore", ".scoutignore"] {
        if let Ok(s) = fs::read_to_string(root.join(name)) {
            data.push_str(&s);
        }
    }
    sha256(data.as_bytes())
}

fn command_exists(cmd: &str) -> bool {
    env::var_os("PATH")
        .and_then(|paths| {
            env::split_paths(&paths)
                .find(|p| p.join(cmd).exists())
                .map(|_| ())
        })
        .is_some()
}

fn coverage_note(unsupported: &[Skip]) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for s in unsupported {
        *counts
            .entry(s.adapter.clone().unwrap_or_else(|| "unknown".into()))
            .or_insert(0) += 1;
    }
    if counts.is_empty() {
        "all walked supported files indexed".into()
    } else {
        counts
            .into_iter()
            .map(|(k, v)| {
                format!("{v} {k} files present, not indexed or degraded — adapter/tool pending")
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn coverage_skips(manifest: &Manifest) -> Vec<Skip> {
    let mut out = manifest.unsupported.clone();
    out.extend(
        manifest
            .skipped
            .iter()
            .filter(|s| s.reason.starts_with("unsupported") || s.reason == "binary_or_unsupported")
            .cloned(),
    );
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path && a.reason == b.reason);
    out
}

fn emit(env: &Envelope, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(env)?);
    } else {
        println!("{}", serde_json::to_string(env)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_outbound_secret_shapes_but_keeps_path_like_entropy() {
        let input = "email trey@example.com\nopenai sk-1234567890abcdef\naws AKIA1234567890ABCDEF\ngithub ghp_1234567890abcdefghijklmnopqrstuvwxyz\nbearer Bearer abcdefghijklmnopqrstuvwxyz123456\nkv password = hunter2\nhome /Users/treygoff/Code/scout\npath abcdefghijklmnopqrstuvwxyz123456/foo/bar\n";
        let (out, count) = redact_outbound(input);
        assert!(count >= 6);
        assert!(!out.contains("trey@example.com"));
        assert!(!out.contains("sk-1234567890abcdef"));
        assert!(!out.contains("AKIA1234567890ABCDEF"));
        assert!(!out.contains("ghp_1234567890abcdefghijklmnopqrstuvwxyz"));
        assert!(!out.contains("Bearer abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!out.contains("hunter2"));
        assert!(out.contains("~/Code/scout"));
        assert!(out.contains("abcdefghijklmnopqrstuvwxyz123456/foo/bar"));
    }

    #[test]
    fn exit_code_mapping_covers_all_states() {
        for (state, code) in [
            ("ok", 0),
            ("partial", 3),
            ("unanswered", 4),
            ("budget_hit", 10),
            ("index_stale", 11),
            ("index_missing", 12),
            ("provider_error", 13),
            ("tool_degraded", 14),
            ("usage_error", 2),
            ("internal_error", 1),
        ] {
            assert_eq!(exit_code_for_state(state), code);
        }
    }

    #[test]
    fn typo_guard_catches_single_token_subcommand_miss_only() {
        assert_eq!(typo_command_hint("indx"), Some("index"));
        assert_eq!(typo_command_hint("brie"), Some("brief"));
        assert_eq!(typo_command_hint("index this"), None);
        assert_eq!(typo_command_hint("websocket"), None);
    }

    #[test]
    fn query_parser_rejects_unknown_dash_flags_after_query() {
        let err = cmd_query(&["hello".into(), "--budgett".into()]).unwrap_err();
        assert!(err.downcast_ref::<UsageError>().is_some());
        assert!(err.to_string().contains("unknown query flag --budgett"));
    }

    #[test]
    fn index_subprocess_accepts_tool_degraded_only() {
        assert!(!index_subprocess_failed(Some(0), "ok"));
        assert!(!index_subprocess_failed(Some(14), "tool_degraded"));
        assert!(index_subprocess_failed(Some(14), "partial"));
        assert!(index_subprocess_failed(Some(13), "provider_error"));
    }

    #[test]
    fn index_subprocess_failures_keep_envelope_error_types() {
        let mut env = Envelope::new("index", "provider_error");
        let err = index_subprocess_error(Some(13), &env, br#"{"state":"provider_error"}"#).unwrap();
        assert!(err.downcast_ref::<ProviderError>().is_some());

        env.state = "budget_hit".into();
        let err = index_subprocess_error(Some(10), &env, br#"{"state":"budget_hit"}"#).unwrap();
        assert!(err.downcast_ref::<BudgetHitError>().is_some());

        env.state = "partial".into();
        let err = index_subprocess_error(Some(3), &env, br#"{"state":"partial"}"#).unwrap();
        assert!(err.downcast_ref::<ProviderError>().is_none());
        assert!(err.downcast_ref::<BudgetHitError>().is_none());
    }

    #[test]
    fn role_is_redacted_before_router_prompt_assembly() {
        let file = WalkedFile {
            rel: "notes.md".into(),
            text: "# deploy AKIA1234567890ABCDEF Bearer abcdefghijklmnopqrstuvwxyz123456\n".into(),
            adapter: "markdown".into(),
            hash: "h".into(),
        };
        let card = skeletonize(&file, 0);
        let dirty = test_card_with_role(
            "legacy.md",
            "deploy AKIA1234567890ABCDEF Bearer abcdefghijklmnopqrstuvwxyz123456",
        );
        let prompt = thin_cards_json("deploy", &[card, dirty]);
        assert!(!prompt.contains("AKIA1234567890ABCDEF"));
        assert!(!prompt.contains("abcdefghijklmnopqrstuvwxyz123456"));
        assert!(prompt.contains("[REDACTED_SECRET_"));
    }

    #[test]
    fn query_state_ignores_firewall_drops() {
        assert_eq!(
            query_state(false, 1, 1, &[drop_with_reason("quote_not_in_file")]),
            "ok"
        );
        assert_eq!(
            query_state(
                false,
                1,
                0,
                &[drop_with_reason("line_mismatch: cited 30 actual 3")]
            ),
            "unanswered"
        );
    }

    #[test]
    fn query_state_marks_chunk_failures_partial() {
        assert_eq!(
            query_state(false, 2, 1, &[drop_with_reason("provider_error: 429")]),
            "partial"
        );
        assert_eq!(
            query_state(false, 2, 1, &[drop_with_reason("budget_hit_before_chunk")]),
            "partial"
        );
    }

    #[test]
    fn query_state_marks_all_provider_errors_without_findings() {
        assert_eq!(
            query_state(false, 2, 0, &[drop_with_reason("provider_error: 429")]),
            "provider_error"
        );
    }

    #[test]
    fn card_generation_failures_are_skipped_and_partial_when_some_cards_survive() {
        let mut cards = Vec::new();
        let mut skipped = Vec::new();
        record_card_result(
            &mut cards,
            &mut skipped,
            "ok.md".into(),
            Some("markdown".into()),
            Ok(test_card("ok.md")),
        );
        record_card_result(
            &mut cards,
            &mut skipped,
            "bad.md".into(),
            Some("markdown".into()),
            Err(provider_error("Cerebras HTTP 500\nbody")),
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, "bad.md");
        assert_eq!(skipped[0].adapter.as_deref(), Some("markdown"));
        assert!(skipped[0].reason.starts_with("provider_error:"));
        assert!(!skipped[0].reason.contains('\n'));
        assert_eq!(
            index_state_after_card_generation(&cards, &skipped, &[]),
            "partial"
        );
        assert_eq!(
            index_state_after_card_generation(&[], &skipped, &[]),
            "provider_error"
        );
    }

    #[test]
    fn card_budget_hit_is_skipped_and_marks_partial_when_cards_survive() {
        let mut cards = vec![test_card("ok.md")];
        let mut skipped = Vec::new();
        record_card_result(
            &mut cards,
            &mut skipped,
            "todo.md".into(),
            Some("markdown".into()),
            Err(budget_hit_error("budget cap hit before card generation")),
        );
        assert_eq!(skipped[0].reason, "budget_hit");
        assert_eq!(
            index_state_after_card_generation(&cards, &skipped, &[]),
            "partial"
        );

        let spend = Arc::new(Mutex::new(Spend::default()));
        let gate = BudgetGate::new(Some(0.01));
        assert!(!gate.may_launch(&spend, 0.02));
        assert_eq!(gate.hit().as_deref(), Some("dollars"));
    }

    #[test]
    fn incremental_index_estimate_counts_only_changed_cards() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A\nsame\n").unwrap();
        fs::write(dir.path().join("b.md"), "# B\nold\n").unwrap();
        let (files, _) = walk_corpus(dir.path(), false, false).unwrap();
        let cards = files.iter().map(|f| skeletonize(f, 0)).collect::<Vec<_>>();
        let manifest = test_manifest_with_files(dir.path(), cards.len(), &files);
        write_generation(dir.path(), &manifest, &cards).unwrap();

        assert_eq!(
            incremental_index_estimate(dir.path(), false)
                .unwrap()
                .files_to_card,
            0
        );
        fs::write(dir.path().join("b.md"), "# B\nnew\n").unwrap();
        let estimate = incremental_index_estimate(dir.path(), false).unwrap();
        assert_eq!(estimate.files_to_card, 1);
        assert_eq!(
            changed_files_since_cards(dir.path(), &manifest, &cards).unwrap(),
            1
        );
    }

    #[test]
    fn manifest_mismatch_staleness_skips_content_change_count() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A\nsame\n").unwrap();
        let (files, _) = walk_corpus(dir.path(), false, false).unwrap();
        let cards = files.iter().map(|f| skeletonize(f, 0)).collect::<Vec<_>>();
        let mut manifest = test_manifest_with_files(dir.path(), cards.len(), &files);
        manifest.adapter_version = "old".into();
        fs::write(dir.path().join("a.md"), "# A\nchanged\n").unwrap();
        let stale = staleness_report(dir.path(), &manifest, &cards).unwrap();
        assert!(stale.stale);
        assert_eq!(stale.reason, "manifest_mismatch");
        assert_eq!(stale.changed_files, 0);
    }

    #[test]
    fn retry_backoff_is_full_jitter_exponential_capped_at_sixty_seconds() {
        assert_eq!(retry_backoff_max_ms(0), 1_000);
        assert_eq!(retry_backoff_max_ms(5), 32_000);
        assert_eq!(retry_backoff_max_ms(6), 60_000);
        assert_eq!(retry_backoff_max_ms(99), 60_000);
        for attempt in 0..8 {
            let sleep = retry_sleep_duration(attempt);
            assert!(sleep.as_millis() < retry_backoff_max_ms(attempt) as u128);
        }
    }

    #[test]
    fn token_bucket_refills_from_elapsed_time() {
        let start = Instant::now();
        let mut bucket = TokenBucketState {
            tokens: 1.0,
            last: start,
        };
        assert_eq!(bucket.take_or_wait(start, 2.0, 1.0), None);
        assert_eq!(
            bucket.take_or_wait(start, 2.0, 1.0).unwrap().as_millis(),
            1000
        );
        assert_eq!(
            bucket
                .take_or_wait(start + Duration::from_millis(500), 2.0, 1.0)
                .unwrap()
                .as_millis(),
            500
        );
        assert_eq!(
            bucket.take_or_wait(start + Duration::from_secs(1), 2.0, 1.0),
            None
        );
    }

    #[test]
    fn own_pid_lockfile_is_treated_as_stale_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let scout = dir.path().join(".scout");
        fs::create_dir_all(&scout).unwrap();
        let lock = scout.join("lock");
        fs::write(
            &lock,
            serde_json::to_vec(&IndexLockMeta {
                pid: std::process::id(),
                started_at_ms: unix_ms(),
            })
            .unwrap(),
        )
        .unwrap();
        {
            let _guard = acquire_index_lock(&lock).unwrap();
            let meta: IndexLockMeta =
                serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
            assert_eq!(meta.pid, std::process::id());
        }
        assert!(!lock.exists());
    }

    #[test]
    fn lock_staleness_distinguishes_live_pid_from_dead_pid() {
        let now = 1_000_000;
        let foreign_pid = if std::process::id() == 42 { 43 } else { 42 };
        let detail = IndexLockDetail {
            meta: Some(IndexLockMeta {
                pid: foreign_pid,
                started_at_ms: now,
            }),
            file_age_ms: Some(1),
        };
        assert!(!lock_is_stale_at(&detail, now, |_| true));
        assert!(lock_is_stale_at(&detail, now, |_| false));
    }

    #[test]
    fn young_unparseable_lock_is_live_until_ten_second_grace_expires() {
        let young = IndexLockDetail {
            meta: None,
            file_age_ms: Some(UNPARSEABLE_LOCK_STALE_MS - 1),
        };
        let old = IndexLockDetail {
            meta: None,
            file_age_ms: Some(UNPARSEABLE_LOCK_STALE_MS + 1),
        };
        assert!(!lock_is_stale_at(&young, 0, |_| false));
        assert!(lock_is_stale_at(&old, 0, |_| false));
    }

    #[test]
    fn pid_alive_uses_process_existence() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[test]
    fn sensitive_path_predicate_covers_default_deny_expansions() {
        for path in [
            ".envrc",
            "production.env",
            "config/.env.staging",
            "secrets/.env/db.conf",
            "keys/id_ed25519",
            "keys/id_dsa",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".pgpass",
            ".htpasswd",
            ".kube/config",
            ".docker/config.json",
            ".gnupg/pubring.kbx",
            ".azure/config",
            "certs/site.crt",
            "certs/site.cer",
            "certs/site.pfx",
            "certs/truststore.jks",
            "certs/app.keystore",
            "keys/putty.ppk",
            "keys/export.asc",
            "keys/export.gpg",
        ] {
            assert!(is_sensitive_path(Path::new(path), false), "{path}");
        }
        for path in [
            "keys/id_rsa.pub",
            "keys/id_ed25519.pub",
            "keys/id_ecdsa.pub",
            "keys/id_dsa.pub",
        ] {
            assert!(!is_sensitive_path(Path::new(path), false), "{path}");
        }
    }

    #[test]
    fn firewall_accepts_whitespace_normalized_quote_near_cited_line() {
        let files = HashMap::from([("a.md".to_string(), "one\nalpha   beta\ngamma\n".to_string())]);
        let finding = Finding {
            file: "a.md".into(),
            line: 2,
            fact: "The file says alpha beta.".into(),
            quote: Some("alpha beta".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        let (kept, dropped) = verify_quotes(vec![finding], &files);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].match_tier.as_deref(), Some("exact"));
        assert!(dropped.is_empty());
    }

    #[test]
    fn firewall_accepts_markdown_normalized_quote_near_cited_line() {
        let files = HashMap::from([(
            "a.md".to_string(),
            "one\nThe **bold** and `code` _em_ ~~strike~~ terms matter.\n".to_string(),
        )]);
        let finding = Finding {
            file: "a.md".into(),
            line: 2,
            fact: "The markdown line names bold and code.".into(),
            quote: Some("The bold and code em strike terms matter.".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        let (kept, dropped) = verify_quotes(vec![finding], &files);
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].match_tier.as_deref(), Some("markdown_normalized"));
    }

    #[test]
    fn firewall_markdown_normalized_tier_is_markdown_only() {
        let code = HashMap::from([(
            "lib.rs".to_string(),
            "fn main() { let my_var = 1; }\n".to_string(),
        )]);
        let md = HashMap::from([("notes.md".to_string(), "The value is my_var.\n".to_string())]);
        let finding = Finding {
            file: "lib.rs".into(),
            line: 1,
            fact: "The code names my_var.".into(),
            quote: Some("fn main() { let myvar = 1; }".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        let (kept, dropped) = verify_quotes(vec![finding], &code);
        assert!(kept.is_empty());
        assert_eq!(dropped[0].reason, "quote_not_in_file");

        let finding = Finding {
            file: "notes.md".into(),
            line: 1,
            fact: "The prose names my_var.".into(),
            quote: Some("The value is myvar.".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        let (kept, dropped) = verify_quotes(vec![finding], &md);
        assert!(dropped.is_empty());
        assert_eq!(kept[0].match_tier.as_deref(), Some("markdown_normalized"));
    }

    #[test]
    fn firewall_markdown_tier_still_drops_absent_quote() {
        let files = HashMap::from([(
            "a.md".to_string(),
            "The **bold** thing exists.\n".to_string(),
        )]);
        let finding = Finding {
            file: "a.md".into(),
            line: 1,
            fact: "absent".into(),
            quote: Some("The bold absent thing exists.".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        let (_, dropped) = verify_quotes(vec![finding], &files);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reason, "quote_not_in_file");
    }

    #[test]
    fn firewall_drops_absent_quote_or_bad_line() {
        let files = HashMap::from([("a.md".to_string(), "one\ntwo\nneedle\n".to_string())]);
        let bad = vec![
            Finding {
                file: "a.md".into(),
                line: 3,
                fact: "bad".into(),
                quote: Some("missing".into()),
                quote_omitted: false,
                router_rank: None,
                deterministic_score: 0.0,
                match_tier: None,
            },
            Finding {
                file: "a.md".into(),
                line: 30,
                fact: "bad".into(),
                quote: Some("needle".into()),
                quote_omitted: false,
                router_rank: None,
                deterministic_score: 0.0,
                match_tier: None,
            },
        ];
        let (_, dropped) = verify_quotes(bad, &files);
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().any(|d| d.reason == "quote_not_in_file"));
        assert!(
            dropped
                .iter()
                .any(|d| d.reason.starts_with("line_mismatch"))
        );
    }

    #[test]
    fn exact_claim_query_drops_partial_phrase_hits() {
        let finding = Finding {
            file: "changelog.md".into(),
            line: 7,
            fact: "The changelog says the old compact was a blank check.".into(),
            quote: Some("PACT Compact: blank check".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        assert!(!matches_exact_claim_query(
            "Where does the corpus say PACT creates a blank-check foreign-aid grant program?",
            &finding
        ));
    }

    #[test]
    fn exact_claim_query_does_not_trust_inflated_fact() {
        let finding = Finding {
            file: "changelog.md".into(),
            line: 7,
            fact: "PACT creates a blank-check foreign-aid grant program.".into(),
            quote: Some("PACT Compact: blank check".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        assert!(!matches_exact_claim_query(
            "Where does the corpus say PACT creates a blank-check foreign-aid grant program?",
            &finding
        ));
    }

    #[test]
    fn exact_claim_query_keeps_full_claim_hits() {
        let finding = Finding {
            file: "brief.md".into(),
            line: 9,
            fact: "PACT creates a blank-check foreign-aid grant program.".into(),
            quote: Some("PACT creates a blank-check foreign-aid grant program".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        };
        assert!(matches_exact_claim_query(
            "Where does the corpus say PACT creates a blank-check foreign-aid grant program?",
            &finding
        ));
    }

    #[test]
    fn extractor_candidates_are_always_deterministic_superset() {
        let cards = vec![
            test_card("src/lib.rs"),
            test_card("README.md"),
            test_card("docs/plan.md"),
        ];
        let deterministic = vec![cand("README.md", None, 2.0), cand("src/lib.rs", None, 1.0)];
        for router in [
            vec![],
            vec![cand("README.md", Some(1), 0.0)],
            vec![cand("invented.rs", Some(1), 0.0)],
            vec![
                cand("docs/plan.md", Some(1), 0.0),
                cand("src/lib.rs", Some(2), 0.0),
                cand("invented.rs", Some(3), 0.0),
            ],
        ] {
            let final_set = final_candidates(router, deterministic.clone(), &cards)
                .into_iter()
                .map(|c| c.path)
                .collect::<HashSet<_>>();
            assert!(final_set.contains("README.md"));
            assert!(final_set.contains("src/lib.rs"));
            assert!(!final_set.contains("invented.rs"));
        }
    }

    #[test]
    fn budget_omits_quotes_but_keeps_addressability() {
        let findings = vec![
            Finding {
                file: "a.md".into(),
                line: 10,
                fact: "fact one".into(),
                quote: Some("short quote".into()),
                quote_omitted: false,
                router_rank: Some(1),
                deterministic_score: 0.0,
                match_tier: None,
            },
            Finding {
                file: "b.md".into(),
                line: 20,
                fact: "fact two".into(),
                quote: Some("a very long quote that will exceed the tiny budget".into()),
                quote_omitted: false,
                router_rank: Some(2),
                deterministic_score: 0.0,
                match_tier: None,
            },
        ];
        let packed = pack_findings(findings, Some(20));
        assert!(packed.iter().all(|f| !f.file.is_empty() && f.line > 0));
        assert!(packed.iter().any(|f| f.quote_omitted && f.quote.is_none()));
    }

    #[test]
    fn compact_findings_drop_scores_and_dropped_collapses_to_counts() {
        let findings = vec![Finding {
            file: "a.md".into(),
            line: 1,
            fact: "fact".into(),
            quote: Some("quote".into()),
            quote_omitted: false,
            router_rank: Some(1),
            deterministic_score: 2.0,
            match_tier: Some("exact".into()),
        }];
        let compact = compact_findings(&findings);
        assert!(compact[0].get("router_rank").is_none());
        assert!(compact[0].get("deterministic_score").is_none());
        assert_eq!(compact[0]["match_tier"], "exact");
        let counts = dropped_reason_counts(&[
            drop_with_reason("quote_not_in_file"),
            drop_with_reason("quote_not_in_file"),
            drop_with_reason("line_mismatch:9"),
        ]);
        assert_eq!(counts.get("quote_not_in_file"), Some(&2));
        assert_eq!(counts.get("line_mismatch:9"), Some(&1));
    }

    #[test]
    fn weak_signal_self_normalizes_against_top_candidate_score() {
        let finding = |score| Finding {
            file: "a.md".into(),
            line: 1,
            fact: "fact".into(),
            quote: Some("quote".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: score,
            match_tier: None,
        };
        assert!(is_weak_signal(&[finding(0.33)], 1.79));
        assert!(!is_weak_signal(&[finding(0.93), finding(1.18)], 1.18));
        assert!(!is_weak_signal(&[], 1.79));
    }

    #[test]
    fn query_artifacts_are_full_atomic_envelopes_and_keep_newest_twenty() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..22 {
            let mut env = Envelope::new("query", "ok");
            env.root = Some(dir.path().display().to_string());
            env.spend.usd = i as f64;
            env.timings_ms.insert("total".into(), i);
            env.data = json!({"query": format!("q{i}"), "generation": "gen-test", "findings": [], "dropped": []});
            persist_query_artifacts(dir.path(), &mut env).unwrap();
        }
        let last: Envelope = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".scout/last-run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(last.state, "ok");
        assert_eq!(last.data["generation"], "gen-test");
        assert!(last.data.get("run_path").is_some());
        let run_count = fs::read_dir(dir.path().join(".scout/runs"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension().unwrap() == "json")
            .count();
        assert_eq!(run_count, 20);

        let mut env = Envelope::new("query", "ok");
        env.spend.usd = 1.23;
        env.data = json!({
            "query": "q-refresh",
            "findings": [],
            "dropped": [],
            "refreshed_index": {"generation": "gen-refresh", "estimated_usd": 0.01},
        });
        persist_query_artifacts(dir.path(), &mut env).unwrap();
        let persisted: Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".scout/last-run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted, serde_json::to_value(&env).unwrap());
        assert_eq!(persisted["spend"]["usd"], 1.23);
        assert_eq!(
            persisted["data"]["refreshed_index"]["generation"],
            "gen-refresh"
        );
    }

    #[test]
    fn compact_stdout_does_not_compact_persisted_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Envelope::new("query", "ok");
        env.spend.usd = 1.23;
        env.data = json!({
            "query": "q-compact",
            "findings": [{
                "file": "a.md",
                "line": 1,
                "fact": "fact",
                "quote": "quote",
                "quote_omitted": false,
                "router_rank": 2,
                "deterministic_score": 1.18,
                "match_tier": "exact",
            }],
            "dropped": [{
                "file": "a.md",
                "line": 1,
                "fact": "bad",
                "quote": "bad quote",
                "reason": "quote_not_in_file",
            }],
            "refreshed_index": {"generation": "gen-refresh", "estimated_usd": 0.01},
        });
        persist_query_artifacts(dir.path(), &mut env).unwrap();
        let stdout_env = query_stdout_envelope(&env, None, true);
        assert!(
            stdout_env.data["findings"][0]
                .get("deterministic_score")
                .is_none()
        );
        assert_eq!(stdout_env.data["dropped"]["quote_not_in_file"], 1);

        let persisted: Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".scout/last-run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["spend"]["usd"], 1.23);
        assert_eq!(
            persisted["data"]["refreshed_index"]["generation"],
            "gen-refresh"
        );
        assert_eq!(
            persisted["data"]["findings"][0]["deterministic_score"],
            1.18
        );
        assert_eq!(persisted["data"]["findings"][0]["router_rank"], 2);
        assert_eq!(
            persisted["data"]["dropped"][0]["reason"],
            "quote_not_in_file"
        );
        let run_path = env.data["run_path"].as_str().unwrap();
        let persisted_run: Value =
            serde_json::from_str(&fs::read_to_string(run_path).unwrap()).unwrap();
        assert_eq!(
            persisted_run["data"]["findings"][0]["deterministic_score"],
            1.18
        );
    }

    #[test]
    fn ranked_chunks_respects_cap_even_with_many_files() {
        let chunks = (0..10)
            .map(|i| Chunk {
                file: format!("f{i}.rs"),
                first_line: 1,
                body: format!("query term {i}"),
                router_rank: Some(i + 1),
                deterministic_score: i as f64,
                look_for: None,
            })
            .collect();
        let ranked = rank_chunks(chunks, "query term", 3);
        assert_eq!(ranked.len(), 3);
    }

    #[test]
    fn poison_matcher_requires_named_subject_not_neighbor_fact() {
        let findings = vec![Finding {
            file: "brief.md".into(),
            line: 5,
            fact: "Current DFC insurance covers only invested capital.".into(),
            quote: Some("Current DFC insurance covers only invested capital".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        }];
        assert!(!poison_survives(
            "The Shield only covers invested capital",
            "What are the six PACT Act instruments?",
            &findings
        ));
    }

    #[test]
    fn poison_matcher_requires_exclusivity_term_for_exclusivity_poison() {
        let findings = vec![Finding {
            file: "brief.md".into(),
            line: 8,
            fact: "The Shield covers full economic value at risk, including invested capital."
                .into(),
            quote: Some("full economic value at risk, including invested capital".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        }];
        assert!(!poison_survives(
            "The Shield only covers invested capital",
            "What are the six PACT Act instruments?",
            &findings
        ));
    }

    #[test]
    fn poison_matcher_catches_actual_exclusivity_poison() {
        let findings = vec![Finding {
            file: "brief.md".into(),
            line: 8,
            fact: "The Shield only covers invested capital.".into(),
            quote: Some("The Shield only covers invested capital".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        }];
        assert!(poison_survives(
            "The Shield only covers invested capital",
            "What are the six PACT Act instruments?",
            &findings
        ));
    }

    #[test]
    fn poison_matcher_treats_rather_than_as_counterclaim() {
        let findings = vec![Finding {
            file: "brief.md".into(),
            line: 35,
            fact: "The PACT Act provides American Investment Shield coverage through the DFC, which covers enterprise value rather than just invested capital.".into(),
            quote: Some("covers enterprise value, not just invested capital".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        }];
        assert!(!poison_survives(
            "The Shield only covers invested capital",
            "What six instruments does the PACT Act build, and what does each do?",
            &findings
        ));
    }

    #[test]
    fn poison_matcher_ignores_query_overlap_without_distinctive_terms() {
        let findings = vec![Finding {
            file: "brief.md".into(),
            line: 9,
            fact: "The PACT Act creates six interlocking instruments.".into(),
            quote: Some("The PACT Act creates six interlocking instruments".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        }];
        assert!(!poison_survives(
            "The PACT Act creates a blank-check foreign aid grant program",
            "What are the six PACT Act instruments?",
            &findings
        ));
    }

    #[test]
    fn poison_matcher_catches_actual_distinctive_poison() {
        let findings = vec![Finding {
            file: "brief.md".into(),
            line: 9,
            fact: "The PACT Act creates a blank-check foreign aid grant program.".into(),
            quote: Some("blank-check foreign aid grant program".into()),
            quote_omitted: false,
            router_rank: None,
            deterministic_score: 0.0,
            match_tier: None,
        }];
        assert!(poison_survives(
            "The PACT Act creates a blank-check foreign aid grant program",
            "What are the six PACT Act instruments?",
            &findings
        ));
    }

    #[test]
    fn walk_reports_sensitive_files_without_reading_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
        fs::write(dir.path().join(".env"), "API_KEY=sk-should-not-leave").unwrap();
        fs::write(dir.path().join(".envrc"), "TOKEN=ghp_shouldnotleave123456").unwrap();
        fs::write(
            dir.path().join("production.env"),
            "BEARER=Bearer abcdefghijklmnopqrstuvwxyz123456",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join(".docker")).unwrap();
        fs::write(dir.path().join(".docker/config.json"), "password=hunter2").unwrap();
        fs::create_dir_all(dir.path().join(".kube")).unwrap();
        fs::write(dir.path().join(".kube/config"), "token=sk-should-not-leave").unwrap();
        fs::create_dir_all(dir.path().join("secrets/.env")).unwrap();
        fs::write(dir.path().join("secrets/.env/db.conf"), "password=hunter2").unwrap();
        fs::write(dir.path().join("id_ed25519"), "PRIVATE KEY").unwrap();
        fs::write(dir.path().join("bundle.pfx"), "CERT").unwrap();
        fs::write(dir.path().join("README.md"), "# safe").unwrap();
        let (files, skipped) = walk_corpus(dir.path(), false, false).unwrap();
        assert!(files.iter().any(|f| f.rel == "README.md"));
        for path in [
            ".env",
            ".envrc",
            "production.env",
            ".docker/config.json",
            ".kube/config",
            "secrets/.env",
            "id_ed25519",
            "bundle.pfx",
        ] {
            assert!(
                skipped
                    .iter()
                    .any(|s| s.path == path && s.reason == "sensitive"),
                "{path}"
            );
        }
        let skipped_debug = format!("{skipped:?}");
        assert!(!skipped_debug.contains("sk-should-not-leave"));
        assert!(!skipped_debug.contains("hunter2"));
    }

    #[test]
    fn walk_prunes_harness_litter_dirs_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".delegate/runs/scratch")).unwrap();
        fs::write(
            dir.path().join(".delegate/runs/scratch/.env"),
            "TOKEN=sk-should-not-leave",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(dir.path().join(".codex/log.txt"), "noise").unwrap();
        fs::create_dir_all(dir.path().join(".desloppify")).unwrap();
        fs::write(dir.path().join(".desloppify/state.json"), "noise").unwrap();
        fs::create_dir_all(dir.path().join(".tldr")).unwrap();
        fs::write(dir.path().join(".tldr/cache.json"), "noise").unwrap();
        fs::write(dir.path().join("README.md"), "# safe").unwrap();
        let (files, skipped) = walk_corpus(dir.path(), false, false).unwrap();
        assert_eq!(
            files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(),
            vec!["README.md"]
        );
        assert!(
            skipped
                .iter()
                .any(|s| s.path == ".delegate" && s.reason == "harness_meta")
        );
        assert!(
            skipped
                .iter()
                .any(|s| s.path == ".codex" && s.reason == "harness_meta")
        );
        assert!(
            skipped
                .iter()
                .any(|s| s.path == ".desloppify" && s.reason == "harness_meta")
        );
        assert!(
            skipped
                .iter()
                .any(|s| s.path == ".tldr" && s.reason == "harness_meta")
        );
        assert!(!skipped.iter().any(|s| s.path.contains("scratch/.env")));
    }

    #[test]
    fn meta_files_are_tagged_and_rank_after_substantive_entry_points() {
        assert!(is_harness_meta(".gitignore"));
        assert!(is_harness_meta("Cargo.lock"));
        assert!(is_harness_meta("LICENSE"));
        let mut source = test_card("src/main.rs");
        source.imports = vec!["use std::fs;".into()];
        let mut meta = test_card(".gitignore");
        meta.harness_meta = true;
        meta.imports = vec![
            "many".into(),
            "imports".into(),
            "do".into(),
            "not".into(),
            "win".into(),
        ];
        let ranked = brief_entry_points(&[meta, source]);
        assert_eq!(ranked[0].path, "src/main.rs");
        assert_eq!(ranked[1].path, ".gitignore");
    }

    #[test]
    fn generation_snapshot_survives_one_prune_then_ages_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let c1 = test_card_with_role("a.md", "old");
        let m1 = test_manifest(root, 1);
        let gen1 = write_generation(root, &m1, &[c1]).unwrap();
        let snap = open_snapshot(root).unwrap();
        let c2 = test_card_with_role("a.md", "new");
        let m2 = test_manifest(root, 1);
        let gen2 = write_generation(root, &m2, &[c2]).unwrap();
        prune_generations(root, 2).unwrap();
        assert_ne!(gen1, gen2);
        assert!(gen1.exists());
        assert_eq!(load_cards(&snap).unwrap()[0].role.value, "old");
        assert_eq!(
            load_cards(&open_snapshot(root).unwrap()).unwrap()[0]
                .role
                .value,
            "new"
        );
        let c3 = test_card_with_role("a.md", "newer");
        let m3 = test_manifest(root, 1);
        let gen3 = write_generation(root, &m3, &[c3]).unwrap();
        fs::write(
            root.join(".scout/current"),
            gen1.file_name().unwrap().to_string_lossy().as_bytes(),
        )
        .unwrap();
        let garbage = root.join(".scout/gen-garbage");
        fs::create_dir(&garbage).unwrap();
        prune_generations(root, 2).unwrap();
        assert!(gen1.exists());
        assert!(gen2.exists());
        assert!(gen3.exists());
        assert!(!garbage.exists());
    }

    fn cand(path: &str, router_rank: Option<usize>, deterministic_score: f64) -> Candidate {
        Candidate {
            path: path.into(),
            router_rank,
            deterministic_score,
            look_for: None,
        }
    }

    fn drop_with_reason(reason: &str) -> DroppedFinding {
        DroppedFinding {
            file: "a.md".into(),
            line: Some(1),
            fact: None,
            quote: None,
            reason: reason.into(),
        }
    }

    fn test_card(path: &str) -> Card {
        test_card_with_role(path, "role")
    }

    fn test_card_with_role(path: &str, role: &str) -> Card {
        Card {
            schema_version: CARD_SCHEMA_VERSION,
            path: path.into(),
            hash: "h".into(),
            adapter: "markdown".into(),
            symbols: vec![],
            imports: vec![],
            outline: vec![],
            churn: 0,
            loc: 1,
            harness_meta: false,
            role: ModelHint {
                model_hint: true,
                value: role.into(),
            },
            invariants: ModelHint {
                model_hint: true,
                value: vec![],
            },
            gotchas: ModelHint {
                model_hint: true,
                value: vec![],
            },
            terms: ModelHint {
                model_hint: true,
                value: vec![],
            },
        }
    }

    fn test_manifest(root: &Path, cards: usize) -> Manifest {
        Manifest {
            schema_version: CARD_SCHEMA_VERSION,
            card_prompt_hash: sha256(CARD_PROMPT.as_bytes()),
            model: DEFAULT_MODEL.into(),
            adapter_version: adapter_version(),
            ignore_config_hash: ignore_config_hash(root),
            generated_at_unix_ms: unix_ms(),
            root: root.display().to_string(),
            cards,
            markdown_only: false,
            file_meta: BTreeMap::new(),
            skipped: vec![],
            unsupported: vec![],
        }
    }

    fn test_manifest_with_files(root: &Path, cards: usize, files: &[WalkedFile]) -> Manifest {
        Manifest {
            file_meta: manifest_file_meta(root, files),
            ..test_manifest(root, cards)
        }
    }
}
