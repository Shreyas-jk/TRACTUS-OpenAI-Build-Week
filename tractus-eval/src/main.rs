//! `tractus-eval` — offline eval harness for Tractus's LLM surface.
//!
//! For each case in the versioned dataset it runs the real product intent
//! extraction, applies deterministic must-checks, then an LLM judge (rubric,
//! structured output). A case passes when enough repeats clear both gates.
//! Non-zero exit on any failing case, so CI can gate on it.
//!
//! This never touches the enforcement path — it evaluates the non-deterministic
//! model outputs (intent extraction) that the deterministic engine consumes.

mod checks;
mod judge;

use checks::{all_passed, deterministic_checks, load_cases, Case, CheckResult};
use judge::{judge_contract, Judgment};
use serde_json::{json, Value};
use std::env;
use std::path::PathBuf;
use std::process;
use tractus_console::intent::extract_intent;

#[tokio::main]
async fn main() {
    load_dotenv();
    let report_path = match parse_args() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("tractus-eval: {message}");
            process::exit(2);
        }
    };
    if env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        eprintln!("tractus-eval: OPENAI_API_KEY is not set — add it to .env or export it.");
        process::exit(2);
    }

    let config = Config::from_env();
    let http = reqwest::Client::new();
    let cases = load_cases();

    let intent_model = env::var("INTENT_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".to_owned());
    let judge_model = env::var("JUDGE_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".to_owned());
    println!(
        "Tractus intent-extraction eval — {} cases · {} repeat(s) · min score {} · min pass {}",
        cases.len(),
        config.repeats,
        config.min_score,
        config.min_pass
    );
    println!("  intent model: {intent_model}   judge model: {judge_model}\n");

    let mut case_reports = Vec::new();
    let mut passed_cases = 0usize;

    for case in &cases {
        let mut reps = Vec::new();
        let mut pass_count = 0usize;
        for _ in 0..config.repeats {
            let rep = run_rep(&http, case, config.min_score).await;
            if rep.passed {
                pass_count += 1;
            }
            reps.push(rep);
        }
        let passed = pass_count >= config.min_pass;
        if passed {
            passed_cases += 1;
        }
        print_case(case, &reps, pass_count, passed);
        case_reports.push(case_report(case, &reps, pass_count, passed));
    }

    let total = cases.len();
    // Suite-level gate: tolerate occasional non-determinism by requiring at least
    // EVAL_MIN_CASES to pass (defaults to all of them).
    let min_cases = env_usize("EVAL_MIN_CASES", total).min(total);
    let gate = if min_cases < total {
        format!(" (need {min_cases})")
    } else {
        String::new()
    };
    println!("\n==== {passed_cases} / {total} cases passed{gate} ====");

    if let Some(path) = &report_path {
        let report = json!({
            "summary": {
                "cases": total,
                "passed": passed_cases,
                "failed": total - passed_cases,
                "required": min_cases,
                "intent_model": intent_model,
                "judge_model": judge_model,
                "repeats": config.repeats,
                "min_score": config.min_score,
                "min_pass": config.min_pass,
            },
            "cases": case_reports,
        });
        if let Err(error) = std::fs::write(path, format!("{report:#}\n")) {
            eprintln!(
                "tractus-eval: could not write report to {}: {error}",
                path.display()
            );
        } else {
            println!("report written to {}", path.display());
        }
    }

    if passed_cases < min_cases {
        process::exit(1);
    }
}

struct RepResult {
    contract: Option<Value>,
    deterministic: Vec<CheckResult>,
    judge: Result<Judgment, String>,
    passed: bool,
}

async fn run_rep(http: &reqwest::Client, case: &Case, min_score: u8) -> RepResult {
    match extract_intent(http, &case.request).await {
        Ok(contract) => {
            let deterministic = deterministic_checks(case, &contract);
            let deterministic_ok = all_passed(&deterministic);
            let judge = judge_contract(http, &case.request, &contract, &case.rubric_notes).await;
            let judge_ok = judge
                .as_ref()
                .map(|judgment| judgment.min_score() >= min_score)
                .unwrap_or(false);
            RepResult {
                contract: serde_json::to_value(&contract).ok(),
                deterministic,
                passed: deterministic_ok && judge_ok,
                judge,
            }
        }
        Err(error) => RepResult {
            contract: None,
            deterministic: vec![CheckResult {
                label: "extract".to_owned(),
                passed: false,
                detail: format!("extraction failed: {error}"),
            }],
            judge: Err("skipped — extraction failed".to_owned()),
            passed: false,
        },
    }
}

fn print_case(case: &Case, reps: &[RepResult], pass_count: usize, passed: bool) {
    let marker = if passed { "PASS" } else { "FAIL" };
    println!(
        "[{marker}] {}  ({pass_count}/{} reps)",
        case.name,
        reps.len()
    );
    for (index, rep) in reps.iter().enumerate() {
        let failed_checks: Vec<&str> = rep
            .deterministic
            .iter()
            .filter(|check| !check.passed)
            .map(|check| check.label.as_str())
            .collect();
        let det = if failed_checks.is_empty() {
            "det OK".to_owned()
        } else {
            format!("det FAIL [{}]", failed_checks.join(", "))
        };
        let judge = match &rep.judge {
            Ok(judgment) => judgment
                .criteria
                .iter()
                .map(|criterion| format!("{}={}", criterion.name, criterion.score))
                .collect::<Vec<_>>()
                .join(" "),
            Err(error) => format!("judge error: {error}"),
        };
        println!("    rep{}: {det} · {judge}", index + 1);
        // Surface the judge's reasoning for any criterion that missed.
        if let Ok(judgment) = &rep.judge {
            for criterion in &judgment.criteria {
                if criterion.score < 4 {
                    println!(
                        "        {} ({}): {}",
                        criterion.name, criterion.score, criterion.rationale
                    );
                }
            }
        }
    }
}

fn case_report(case: &Case, reps: &[RepResult], pass_count: usize, passed: bool) -> Value {
    let reps_json: Vec<Value> = reps
        .iter()
        .map(|rep| {
            json!({
                "passed": rep.passed,
                "contract": rep.contract,
                "deterministic": rep.deterministic.iter().map(|check| json!({
                    "label": check.label,
                    "passed": check.passed,
                    "detail": check.detail,
                })).collect::<Vec<_>>(),
                "judge": match &rep.judge {
                    Ok(judgment) => serde_json::to_value(judgment).unwrap_or(Value::Null),
                    Err(error) => json!({ "error": error }),
                },
            })
        })
        .collect();
    json!({
        "name": case.name,
        "request": case.request,
        "passed": passed,
        "pass_count": pass_count,
        "reps": reps_json,
    })
}

struct Config {
    repeats: usize,
    min_score: u8,
    min_pass: usize,
}

impl Config {
    fn from_env() -> Self {
        let repeats = env_usize("EVAL_REPEATS", 1).max(1);
        let min_score = env_usize("EVAL_MIN_SCORE", 3).clamp(1, 5) as u8;
        let default_min_pass = repeats.div_ceil(2);
        let min_pass = env_usize("EVAL_MIN_PASS", default_min_pass).clamp(1, repeats);
        Self {
            repeats,
            min_score,
            min_pass,
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

fn parse_args() -> Result<Option<PathBuf>, String> {
    let mut report = None;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--report" => {
                report = Some(PathBuf::from(
                    arguments.next().ok_or("--report requires a path")?,
                ))
            }
            "--help" | "-h" => {
                println!("usage: tractus-eval [--report <path>]");
                println!();
                println!(
                    "Runs the intent-extraction eval (dataset + deterministic checks + LLM judge)."
                );
                println!("Env: OPENAI_API_KEY (required), INTENT_MODEL, JUDGE_MODEL,");
                println!("     EVAL_REPEATS (default 1), EVAL_MIN_SCORE (1-5, default 3),");
                println!(
                    "     EVAL_MIN_PASS (per case), EVAL_MIN_CASES (suite gate, default all)."
                );
                process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(report)
}

/// Load `KEY=VALUE` pairs from `./.env` without overriding the real environment.
fn load_dotenv() {
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().strip_prefix("export ").unwrap_or(key).trim();
        if key.is_empty() || env::var_os(key).is_some() {
            continue;
        }
        env::set_var(key, value.trim().trim_matches('"').trim_matches('\''));
    }
}
