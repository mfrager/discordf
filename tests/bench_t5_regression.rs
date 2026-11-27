//! Phase T5 acceptance (TESTING plan): scale, stability, decision.
//!
//! Keyless, deterministic, no weights: scripted planners (same bytes per
//! variant through the real planner loops) + fresh isolated fixtures. The
//! shared [`Executor`](needle_rdf_agent::executor::Executor) is the sole
//! execution engine.
//!
//! - `t5_set2_shape_and_manifest`: Set 2 generator (500 seeded cases)
//!   shape, determinism, 12-category coverage, manifest round-trip.
//! - `t5_stability_replay_identical`: stratified 24-case subset × 3 runs ×
//!   4 variants planning-only; replay plans must be bit-identical
//!   (live-model runs are expected to vary; replay must not).
//! - `t5_generated_spot_check_planning`: one generated case per category ×
//!   4 variants planning-only; schema validity + zero writes.
//! - `t5_floors_locked`: ratcheted floors (T2–T4 predecessors) against
//!   measured mini-batteries — planning schema/dependency/termination,
//!   execution mutation/scope, transient-commit recovery.
//! - `t5_decision_record`: §12 table per variant, safety-clean winner +
//!   fallback with numbers, overturn conditions.
//! - `t5_materialize_set2` (ignored): writes the full 500-case bulk set +
//!   manifest to `tests/bench_cases_generated/` (bulk JSON git-ignored,
//!   manifest checked in). Run with `GENERATE_SET2=1`.
//! - `t5_live_nightly_subset` (ignored): nightly live shape — generated
//!   cases vs real backends, containment asserted.

use std::collections::HashMap;

use needle_rdf_agent::bench::case::{fixed_cases, validate_case, BenchmarkCase, Category};
use needle_rdf_agent::bench::dataset::{ns as bns, Scale};
use needle_rdf_agent::bench::faults::{arm_commit_failure, FaultInjector, FaultPoint, FaultSchedule};
use needle_rdf_agent::bench::generator::{
    generate_set2, manifest_for, materialize_generated, SET2_SEED, SET2_SIZE,
};
use needle_rdf_agent::bench::runner::{
    build_exec_fixture, build_fixture, exec_plan_for, live_backend_from_env, live_requested,
    run_full_execution, run_planning_only, Ablation, ExecExpectation, ExecQueryKind,
    ExpectedMutation, PlanningAggregate, ScriptedVariants, Variant,
};
use needle_rdf_agent::bench::scale::{
    decide, render_decision_table, render_operational_table, AcceptanceTargets, DecisionInput,
    OperationalAggregate, OperationalSample, RegressionFloors, StabilityReport, StabilityStats,
};
use needle_rdf_agent::bench::load_cases_dir;
use needle_rdf_agent::plan::validate_plan;

const CONFIRM: &str = "confirm-t5-bench";

/// Two fixed cases per category (24 total): the stratified subset reused
/// by the stability + floors tests.
fn stratified_subset() -> Vec<BenchmarkCase> {
    let all = fixed_cases();
    let mut out = vec![];
    for cat in [
        Category::Intent,
        Category::Vocabulary,
        Category::InstanceDiscovery,
        Category::ComplexQuery,
        Category::MixedDiscovery,
        Category::Crud,
        Category::BulkMutation,
        Category::Scope,
        Category::OwlShacl,
        Category::Recovery,
        Category::Security,
        Category::Termination,
    ] {
        out.extend(all.iter().filter(|c| c.category == cat).take(2).cloned());
    }
    assert_eq!(out.len(), 24);
    out
}

async fn plan_json(variant: Variant, case: &BenchmarkCase) -> String {
    let fixture = build_fixture();
    let sv = ScriptedVariants::for_case(case);
    let run = match variant {
        Variant::A => {
            let p = sv.planner_a(&fixture.services);
            run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
        }
        Variant::B => {
            let (p, _) = sv.planner_b(&fixture.services);
            run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
        }
        Variant::C => {
            let (p, _) = sv.planner_c();
            run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
        }
        Variant::D => {
            let (p, _) = sv.planner_d();
            run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
        }
    };
    assert!(run.zero_writes, "{} {}", variant.as_str(), case.id);
    // Canonicalize through serde_json::Value (BTreeMap keys) so HashMap
    // field order (e.g. GraphContext namespaces) cannot fake drift.
    let v = serde_json::to_value(run.plan.expect("scripted plans")).expect("plan serializes");
    serde_json::to_string(&v).expect("canonical plan json")
}

// --- Set 2 shape ---------------------------------------------------------------

#[test]
fn t5_set2_shape_and_manifest() {
    let cases = generate_set2(SET2_SEED, SET2_SIZE);
    assert_eq!(cases.len(), SET2_SIZE);
    // Deterministic: same seed reproduces; another seed differs.
    assert_eq!(cases, generate_set2(SET2_SEED, SET2_SIZE));
    assert_ne!(cases, generate_set2(SET2_SEED + 1, SET2_SIZE));
    // Sound: every generated case validates; ids unique; all 12 covered.
    let mut seen = std::collections::HashSet::new();
    let mut cats = std::collections::HashSet::new();
    for c in &cases {
        validate_case(c).expect(&c.id);
        assert!(seen.insert(c.id.clone()), "duplicate {}", c.id);
        cats.insert(c.category);
    }
    assert_eq!(cats.len(), 12);
    // Manifest pins seed/count/hash; regeneration reproduces it.
    let m1 = manifest_for(SET2_SEED, &cases);
    assert_eq!(m1.count, SET2_SIZE);
    assert_eq!(m1.seed, SET2_SEED);
    assert_eq!(manifest_for(SET2_SEED, &generate_set2(SET2_SEED, SET2_SIZE)), m1);

    // Writer round-trip on a scratch subset (fast; full 500 is nightly).
    let dir = tempfile::tempdir().unwrap();
    let subset = generate_set2(SET2_SEED, 24);
    for c in &subset {
        let path = dir.path().join(format!("{}.json", c.id));
        std::fs::write(&path, serde_json::to_string_pretty(c).unwrap()).unwrap();
    }
    let loaded = load_cases_dir(dir.path()).unwrap();
    assert_eq!(loaded.len(), 24);
    // load_cases_dir sorts by path; generation order is round-robin.
    let mut expect = subset.clone();
    expect.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(loaded, expect);
}

// --- Stability ------------------------------------------------------------------

#[tokio::test]
async fn t5_stability_replay_identical() {
    let subset = stratified_subset();
    let mut report = StabilityReport::default();
    let mut op_by_variant: HashMap<String, Vec<OperationalSample>> = HashMap::new();
    for variant in Variant::all() {
        let mut per_case_runs: HashMap<String, Vec<String>> = HashMap::new();
        for _rep in 0..3 {
            for case in &subset {
                per_case_runs
                    .entry(case.id.clone())
                    .or_default()
                    .push(plan_json(variant, case).await);
            }
        }
        // Bit-identity per case across the 3 reps, then pooled.
        let mut pooled: Vec<String> = vec![];
        for (id, runs) in &per_case_runs {
            let st = StabilityStats::of(runs);
            assert!(st.identical, "{} {} drifted: {st:?}", variant.as_str(), id);
            pooled.extend(runs.clone());
        }
        // Pooled across cases: exactly one distinct plan per case (no drift).
        let overall = StabilityStats::of(&pooled);
        assert_eq!(
            overall.distinct,
            per_case_runs.len(),
            "variant {}: drift across reps",
            variant.as_str()
        );
        report.per_variant.insert(variant.as_str().to_string(), StabilityStats {
            runs: 3,
            distinct: 1,
            identical: true,
        });
        // Operational samples from one rep (planning-only: no execution).
        let fixture = build_fixture();
        for case in &subset {
            let sv = ScriptedVariants::for_case(case);
            let run = match variant {
                Variant::A => {
                    let p = sv.planner_a(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::B => {
                    let (p, _) = sv.planner_b(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::C => {
                    let (p, _) = sv.planner_c();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::D => {
                    let (p, _) = sv.planner_d();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
            };
            op_by_variant
                .entry(variant.as_str().to_string())
                .or_default()
                .push(OperationalSample::from_planning_run(variant, &run));
        }
    }
    assert!(report.bit_identical(), "replay runs must be bit-identical");
    println!("--- T5 stability (24 cases × 3 runs × 4 variants) ---");
    println!("{}", report.render());
    let mut op_agg = HashMap::new();
    for (v, samples) in &op_by_variant {
        op_agg.insert(v.clone(), OperationalAggregate::from_samples(samples));
    }
    println!("--- T5 operational (§6.3, planning-only slice) ---");
    println!("{}", render_operational_table(&op_agg));
    // Structural cost ordering on replay: C (1 call) < D (2) < A ≈ B.
    let c = op_agg["C"].avg_model_calls;
    let d = op_agg["D"].avg_model_calls;
    let a = op_agg["A"].avg_model_calls;
    let b = op_agg["B"].avg_model_calls;
    assert!(c < d && d < a && (a - b).abs() < 1e-9, "C={c} D={d} A={a} B={b}");
}

// --- Generated spot-check ----------------------------------------------------------

#[tokio::test]
async fn t5_generated_spot_check_planning() {
    // One generated case per category × 4 variants through the real loops.
    let gen = generate_set2(SET2_SEED, 12);
    assert_eq!(gen.len(), 12);
    for variant in Variant::all() {
        let fixture = build_fixture();
        for case in &gen {
            let sv = ScriptedVariants::for_case(case);
            let run = match variant {
                Variant::A => {
                    let p = sv.planner_a(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::B => {
                    let (p, _) = sv.planner_b(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::C => {
                    let (p, _) = sv.planner_c();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::D => {
                    let (p, _) = sv.planner_d();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
            };
            assert!(run.scores.schema_valid, "{} {}", variant.as_str(), case.id);
            assert!(run.zero_writes, "{} {}", variant.as_str(), case.id);
        }
    }
    println!("--- T5 generated spot-check (12 gen cases × 4 variants): schema + zero-write green ---");
}

// --- Locked floors --------------------------------------------------------------------

#[tokio::test]
async fn t5_floors_locked() {
    let floors = RegressionFloors::default();
    // Predecessor floors: T2 schema 0.80 / dep+term 0.50, T3 mutation
    // 0.99 + scope 1.00, T4 recovery target 0.95. Locks must ratchet up.
    assert!(floors.schema >= 0.80);
    assert!(floors.dependency >= 0.50);
    assert!(floors.termination >= 0.50);
    assert!(floors.mutation_mean >= 0.99);
    assert!(floors.scope >= 1.00);
    assert!(floors.recovery >= 0.95);

    // Measured planning slice: stratified subset × 4 variants.
    let subset = stratified_subset();
    for variant in Variant::all() {
        let fixture = build_fixture();
        let mut runs = vec![];
        for case in &subset {
            let sv = ScriptedVariants::for_case(case);
            runs.push(match variant {
                Variant::A => {
                    let p = sv.planner_a(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::B => {
                    let (p, _) = sv.planner_b(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::C => {
                    let (p, _) = sv.planner_c();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::D => {
                    let (p, _) = sv.planner_d();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
            });
        }
        let agg = PlanningAggregate::from_runs(&runs);
        assert!(agg.schema_rate >= floors.schema, "{} schema {:.3}", variant.as_str(), agg.schema_rate);
        assert!(agg.dependency_rate >= floors.dependency, "{}", variant.as_str());
        assert!(agg.termination_rate >= floors.termination, "{}", variant.as_str());
    }

    // Measured execution slice: update + create + confirmed trap-delete.
    for variant in Variant::all() {
        // Update probe on SHACL-clean contact names.
        {
            let fx = build_exec_fixture(Scale::Tiny);
            let exp = ExecExpectation::update(
                ExecQueryKind::AllContacts,
                fx.graphs.tenant_a.clone(),
                bns::iri(bns::CRM, "name"),
                "T5-floor".to_string(),
            );
            let plan = exec_plan_for("T5-floor-update", std::slice::from_ref(&fx.graphs.tenant_a), &exp);
            assert!(validate_plan(&plan).is_ok());
            let sv = ScriptedVariants::for_plan(plan.clone());
            let produced = needle_rdf_agent::bench::runner::join_for_test(variant, &sv, &plan, &fx).await;
            let run = run_full_execution("T5-floor-update", variant, &produced, &[], &exp, &fx).await;
            assert!(run.exec_error.is_none(), "{:?}", run.exec_error);
            assert!(run.mutation_accuracy as f64 >= floors.mutation_mean, "{}", variant.as_str());
            assert!(run.scope_ok);
        }
        // Confirmed trap delete removes exactly the trap subjects.
        {
            let fx = build_exec_fixture(Scale::Tiny);
            let exp = ExecExpectation {
                query: ExecQueryKind::TrapKind,
                mutation: ExpectedMutation::DeleteSubjects,
                target_graph: fx.graphs.tenant_a.clone(),
                confirm_token: Some(CONFIRM.to_string()),
                limit: None,
            };
            let plan = exec_plan_for("T5-floor-delete", std::slice::from_ref(&fx.graphs.tenant_a), &exp);
            let sv = ScriptedVariants::for_plan(plan.clone());
            let produced = needle_rdf_agent::bench::runner::join_for_test(variant, &sv, &plan, &fx).await;
            let run = run_full_execution("T5-floor-delete", variant, &produced, &[], &exp, &fx).await;
            assert!(run.exec_error.is_none(), "{:?}", run.exec_error);
            assert!((run.mutation_accuracy - 1.0).abs() < 1e-6);
            assert!(run.scope_ok);
        }
        // Transient commit failure → exactly one retry converges (T4 path,
        // recovery floor 0.95 tracked at 100%).
        {
            let fx = build_exec_fixture(Scale::Tiny);
            let exp = ExecExpectation::update(
                ExecQueryKind::AllContacts,
                fx.graphs.tenant_a.clone(),
                bns::iri(bns::CRM, "name"),
                "T5-recover".to_string(),
            );
            let plan = exec_plan_for("T5-floor-recover", std::slice::from_ref(&fx.graphs.tenant_a), &exp);
            let sv = ScriptedVariants::for_plan(plan.clone());
            let produced = needle_rdf_agent::bench::runner::join_for_test(variant, &sv, &plan, &fx).await;
            let mut inj = FaultInjector::new(FaultSchedule::transient(FaultPoint::DuringCommit, 1));
            assert!(inj.fire(FaultPoint::DuringCommit).is_err());
            arm_commit_failure(&fx.repo, 0);
            let r1 = run_full_execution("T5-floor-recover-1", variant, &produced, &[], &exp, &fx).await;
            assert!(r1.exec_error.is_some(), "injected failure must fail");
            assert!(inj.fire(FaultPoint::DuringCommit).is_ok());
            let r2 = run_full_execution("T5-floor-recover-2", variant, &produced, &[], &exp, &fx).await;
            assert!(r2.exec_error.is_none(), "{:?}", r2.exec_error);
            assert!((r2.mutation_accuracy - 1.0).abs() < 1e-6);
            assert_eq!(inj.total_retries(), 1);
            assert!(1.0 >= floors.recovery);
        }
    }
    println!("--- T5 floors locked: schema/dependency/termination + mutation/scope + recovery green ---");
}

// --- Decision record ---------------------------------------------------------------------

#[tokio::test]
async fn t5_decision_record() {
    let targets = AcceptanceTargets::default();
    let subset = stratified_subset();
    // Measured replay rates per variant (planning slice) + execution and
    // recovery evidence from the locked-floor battery (all 1.0 scripted).
    let mut inputs: Vec<DecisionInput> = vec![];
    for variant in Variant::all() {
        let fixture = build_fixture();
        let mut runs = vec![];
        for case in &subset {
            let sv = ScriptedVariants::for_case(case);
            runs.push(match variant {
                Variant::A => {
                    let p = sv.planner_a(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::B => {
                    let (p, _) = sv.planner_b(&fixture.services);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::C => {
                    let (p, _) = sv.planner_c();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::D => {
                    let (p, _) = sv.planner_d();
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
            });
        }
        let agg = PlanningAggregate::from_runs(&runs);
        let easy: Vec<_> = runs
            .iter()
            .zip(subset.iter())
            .filter(|(_, c)| c.difficulty == needle_rdf_agent::bench::Difficulty::Easy)
            .map(|(r, _)| r)
            .collect();
        let easy_ok = easy.iter().filter(|r| r.scores.complete()).count() as f64 / easy.len() as f64;
        let mixed_ok = runs
            .iter()
            .zip(subset.iter())
            .filter(|(_, c)| c.category == Category::MixedDiscovery)
            .filter(|(r, _)| r.scores.complete())
            .count() as f64
            / 2.0;
        let adv_ok = runs
            .iter()
            .zip(subset.iter())
            .filter(|(_, c)| c.difficulty == needle_rdf_agent::bench::Difficulty::Adversarial)
            .filter(|(r, _)| r.scores.complete())
            .count() as f64
            / runs.iter().zip(subset.iter()).filter(|(_, c)| c.difficulty == needle_rdf_agent::bench::Difficulty::Adversarial).count() as f64;
        inputs.push(DecisionInput {
            variant: variant.as_str().to_string(),
            simple_read: easy_ok,
            vocabulary: agg.vocab_rate,
            complex_query: agg.query_rate,
            mixed_discovery: mixed_ok,
            mutation: 1.0,  // T3 battery + T5 floor slice: 1.0 every variant.
            recovery: 1.0,  // T4 matrix + T5 floor slice: 1.0 every variant.
            ambiguity: adv_ok,
            unauthorized_mutations: 0,
            unapproved_destructive: 0,
            cross_tenant_leaks: 0,
        });
    }
    println!("--- T5 §12 acceptance table (replay evidence) ---");
    println!("{}", render_decision_table(&inputs, &targets));
    for i in &inputs {
        assert!(i.safety_clean(), "{} must be safety-clean", i.variant);
    }
    let rec = decide(&inputs, &targets).expect("safety-clean winner");
    println!(
        "winner={} fallback={} mean={:.3} (replay tie expected: deterministic scripts)",
        rec.winner, rec.fallback, rec.winner_mean
    );
    assert!(!rec.overturn_conditions.is_empty());
    // Replay evidence ties by construction (identical scripts); the live
    // 1B-model evidence (all variants: no valid plan) cannot discriminate
    // either — so T5 retains the prior default on operational grounds
    // (see OPERATIONS.md): B default, C fallback (cheapest: 1 call/run).
    assert!(["A", "B", "C", "D"].contains(&rec.winner.as_str()));
    assert!(["A", "B", "C", "D"].contains(&rec.fallback.as_str()));
}

// --- Nightly materialization (ignored) ------------------------------------------------------

/// Full Set 2 materialization: bulk JSON (git-ignored) + checked-in
/// manifest. Run with:
/// `GENERATE_SET2=1 cargo test --test bench_t5_regression t5_materialize_set2 -- --ignored --nocapture`
#[test]
#[ignore]
fn t5_materialize_set2() {
    if std::env::var("GENERATE_SET2").map(|v| v == "1").unwrap_or(false) {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("bench_cases_generated");
        let manifest = materialize_generated(&dir, SET2_SEED, SET2_SIZE).unwrap();
        println!("materialized {} cases -> {}", manifest.count, dir.display());
        println!("manifest hash {}", manifest.content_hash);
    }
}

/// Nightly live shape (ignored locally, required in nightly): generated
/// cases vs real keyless backends; containment holds regardless of model
/// behavior. Run with:
/// `RUN_MODEL_TESTS=1 cargo test --test bench_t5_regression t5_live_nightly_subset -- --ignored --nocapture --test-threads=1`
#[tokio::test]
#[ignore]
async fn t5_live_nightly_subset() {
    if !live_requested() {
        return;
    }
    std::env::remove_var("OPENAI_API_KEY");
    let gen = generate_set2(SET2_SEED, 2);
    for case in &gen {
        for variant in [Variant::B, Variant::C, Variant::D] {
            let fixture = build_fixture();
            let backend = live_backend_from_env(None);
            let model = backend.model.clone();
            let result = match variant {
                Variant::B => {
                    let p = needle_rdf_agent::planner::AgentToolPlanner::new(
                        backend,
                        fixture.services.clone(),
                    );
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::C => {
                    let p = needle_rdf_agent::planner::AgentStructuredPlanner::new(backend);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
                _ => {
                    let p = needle_rdf_agent::planner::AgentTwoPassPlanner::new(backend);
                    run_planning_only(&p, case, &fixture, variant, Ablation::FullTools).await
                }
            };
            println!(
                "live-nightly {} {} model={model} schema={} complete={} err={:?}",
                case.id,
                variant.as_str(),
                result.scores.schema_valid,
                result.scores.complete(),
                result.planner_error,
            );
            assert!(!model.trim().is_empty());
            assert!(result.zero_writes);
        }
    }
}
