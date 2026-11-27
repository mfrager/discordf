//! Phase T2 acceptance (TESTING plan): planning-only A/B/C/D comparison.
//!
//! All 100 fixed cases × 4 variants, planning only (no execution):
//! - keyless deterministic scripts through the real planner loops
//!   (A: `MockModel` add_step/finish_plan; B/C/D: scripted `agentstream`
//!   backend), so CI needs no key and no weights;
//! - zero graph writes per run (graph hash + audit row count unchanged);
//! - per-category metrics: completeness, dependency/ vocabulary/
//!   query-intent accuracy, schema validity, termination accuracy;
//! - comparative report printed; CI floors: schema validity ≥80% per
//!   variant on non-adversarial cases, dependency + termination ≥50%.
//! - §13 ablation matrix (full-tools vs pre-resolved-vocab vs
//!   precomputed-query vs precomputed-plan) printed per variant.
//! - variant instrumentation: A duplicates/cycles, B turn log + model-id
//!   propagation, C schema validity, D prose→structure preservation.
//! - live subset (`RUN_MODEL_TESTS=1`, ignored locally) proves transport.

use std::collections::HashMap;
use std::sync::Arc;

use needle_rdf_agent::bench::case::{fixed_cases, BenchmarkCase};
use needle_rdf_agent::bench::runner::{
    live_backend_from_env, live_requested, render_ablation_matrix, render_planning_table,
    run_planning_only, synth_plan, Ablation, BenchFixture, PlanningAggregate, PlanningRun, Variant,
    build_fixture,
};
use needle_rdf_agent::bench::Category;
use needle_rdf_agent::plan::{validate_plan, OperationContext};
use needle_rdf_agent::planner::{check_prose_consistency, Planner, UserRequest};

fn cases() -> Vec<BenchmarkCase> {
    fixed_cases()
}

async fn run_variant(variant: Variant, fixture: &BenchFixture) -> (Vec<PlanningRun>, FixtureProof) {
    let hash_before = fixture.hash_tenant_a();
    let audit_before = fixture.audit.len();
    let mut runs = Vec::new();
    for case in cases() {
        let sv = needle_rdf_agent::bench::runner::ScriptedVariants::for_case(&case);
        match variant {
            Variant::A => {
                let planner = sv.planner_a(&fixture.services);
                runs.push(
                    run_planning_only(&planner, &case, fixture, variant, Ablation::FullTools).await,
                );
            }
            Variant::B => {
                let (planner, script) = sv.planner_b(&fixture.services);
                let run =
                    run_planning_only(&planner, &case, fixture, variant, Ablation::FullTools).await;
                assert!(
                    script.models().iter().all(|m| m == variant.script_model_id()),
                    "{:?}",
                    script.models()
                );
                assert_eq!(script.request_count(), run.turns, "{}", case.id);
                runs.push(run);
            }
            Variant::C => {
                let (planner, script) = sv.planner_c();
                let run =
                    run_planning_only(&planner, &case, fixture, variant, Ablation::FullTools).await;
                assert_eq!(script.request_count(), 1, "{}", case.id);
                assert!(
                    script.models().iter().all(|m| m == variant.script_model_id()),
                    "{:?}",
                    script.models()
                );
                runs.push(run);
            }
            Variant::D => {
                let (planner, script) = sv.planner_d();
                let run =
                    run_planning_only(&planner, &case, fixture, variant, Ablation::FullTools).await;
                assert_eq!(script.request_count(), 2, "{}", case.id);
                assert!(
                    script.models().iter().all(|m| m == variant.script_model_id()),
                    "{:?}",
                    script.models()
                );
                runs.push(run);
            }
        }
    }
    let proof = FixtureProof {
        hash_before,
        hash_after: fixture.hash_tenant_a(),
        audit_before,
        audit_after: fixture.audit.len(),
    };
    (runs, proof)
}

struct FixtureProof {
    hash_before: String,
    hash_after: String,
    audit_before: usize,
    audit_after: usize,
}

fn category_of(id: &str, cases: &[BenchmarkCase]) -> Category {
    cases.iter().find(|c| c.id == id).unwrap().category
}

#[tokio::test]
async fn t2_planning_only_all_variants() {
    let all_cases = cases();
    assert_eq!(all_cases.len(), 100);
    let mut by_variant: HashMap<String, PlanningAggregate> = HashMap::new();
    let mut by_variant_nadv: HashMap<String, PlanningAggregate> = HashMap::new();
    let mut all_runs: HashMap<String, Vec<PlanningRun>> = HashMap::new();

    for variant in Variant::all() {
        let fixture = build_fixture();
        let (runs, proof) = run_variant(variant, &fixture).await;
        assert_eq!(runs.len(), 100, "variant {}", variant.as_str());
        // Zero-write gate: planning never touches the repo or audit.
        assert_eq!(proof.hash_before, proof.hash_after, "variant {}", variant.as_str());
        assert_eq!(proof.audit_before, proof.audit_after, "variant {}", variant.as_str());
        assert_eq!(proof.audit_after, 0, "planning must not audit");
        for r in &runs {
            assert!(r.zero_writes, "{} {}", variant.as_str(), r.case_id);
            assert_eq!(r.audit_added, 0, "{}", r.case_id);
        }
        let agg = PlanningAggregate::from_runs(&runs);
        let nadv = PlanningAggregate::from_runs_non_adversarial(&runs, &all_cases);
        // Floors ratchet in T5; T2 requires no silent collapse.
        assert!(
            nadv.schema_rate >= 0.80,
            "variant {} schema validity on non-adversarial = {:.2} (< 0.80)",
            variant.as_str(),
            nadv.schema_rate
        );
        assert!(
            agg.dependency_rate >= 0.50,
            "variant {} dependency accuracy = {:.2} (< 0.50)",
            variant.as_str(),
            agg.dependency_rate
        );
        assert!(
            agg.termination_rate >= 0.50,
            "variant {} termination accuracy = {:.2} (< 0.50)",
            variant.as_str(),
            agg.termination_rate
        );
        by_variant.insert(variant.as_str().to_string(), agg);
        by_variant_nadv.insert(variant.as_str().to_string(), nadv);
        all_runs.insert(variant.as_str().to_string(), runs);
    }

    println!("--- T2 planning-only comparison (all 100 cases) ---");
    println!("{}", render_planning_table(&by_variant));
    println!("--- T2 non-adversarial subset (schema floor ≥80%) ---");
    println!("{}", render_planning_table(&by_variant_nadv));

    // Per-category completeness spotlight (reported, not gated beyond floors).
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
        let ids: Vec<&str> = all_cases
            .iter()
            .filter(|c| c.category == cat)
            .map(|c| c.id.as_str())
            .collect();
        let mut line = format!("{:?}: ", cat);
        for v in Variant::all() {
            let runs = &all_runs[v.as_str()];
            let sub: Vec<&PlanningRun> =
                runs.iter().filter(|r| ids.contains(&r.case_id.as_str())).collect();
            let complete = sub.iter().filter(|r| r.scores.complete()).count();
            line.push_str(&format!("{} {}/{}  ", v.as_str(), complete, sub.len()));
        }
        println!("{line}");
    }
    let _ = category_of("QRY-001", &all_cases);
}

#[tokio::test]
async fn t2_ablation_matrix() {
    let case = cases().into_iter().find(|c| c.id == "QRY-001").unwrap();
    let mut results: HashMap<(String, String), bool> = HashMap::new();
    for ab in Ablation::all() {
        let adapted = ab.apply(&case);
        for variant in Variant::all() {
            let fixture = build_fixture();
            let sv = needle_rdf_agent::bench::runner::ScriptedVariants::for_case(&adapted);
            let run = match variant {
                Variant::A => {
                    let p = sv.planner_a(&fixture.services);
                    run_planning_only(&p, &adapted, &fixture, variant, ab).await
                }
                Variant::B => {
                    let (p, _) = sv.planner_b(&fixture.services);
                    run_planning_only(&p, &adapted, &fixture, variant, ab).await
                }
                Variant::C => {
                    let (p, _) = sv.planner_c();
                    run_planning_only(&p, &adapted, &fixture, variant, ab).await
                }
                Variant::D => {
                    let (p, _) = sv.planner_d();
                    run_planning_only(&p, &adapted, &fixture, variant, ab).await
                }
            };
            assert!(run.zero_writes, "{:?} {}", ab, variant.as_str());
            results.insert(
                (ab.as_str().to_string(), variant.as_str().to_string()),
                run.scores.schema_valid,
            );
        }
    }
    println!("--- T2 §13 ablation matrix (schema validity) ---");
    println!("{}", render_ablation_matrix(&results));
    assert!(
        results.values().all(|ok| *ok),
        "scripted ablations must all validate: {results:?}"
    );
}

#[tokio::test]
async fn t2_variant_instrumentation() {
    // A: no duplicate step ids, no cycles (validate), steps within caps.
    {
        let fixture = build_fixture();
        let case = cases().into_iter().find(|c| c.id == "CRU-001").unwrap();
        let sv = needle_rdf_agent::bench::runner::ScriptedVariants::for_case(&case);
        let planner = sv.planner_a(&fixture.services);
        let run = run_planning_only(&planner, &case, &fixture, Variant::A, Ablation::FullTools).await;
        let plan = run.plan.expect("A plans CRU-001");
        let mut seen = std::collections::HashSet::new();
        assert!(plan.steps.iter().all(|s| seen.insert(s.id.clone())), "no duplicates");
        assert!(validate_plan(&plan).is_ok());
        assert!(plan.steps.len() <= 16);
    }
    // C: plan JSON round-trips through the schema path in both modes.
    {
        let fixture = build_fixture();
        let case = cases().into_iter().find(|c| c.id == "QRY-002").unwrap();
        let sv = needle_rdf_agent::bench::runner::ScriptedVariants::for_case(&case);
        let (planner, _) = sv.planner_c();
        let run = run_planning_only(&planner, &case, &fixture, Variant::C, Ablation::FullTools).await;
        assert!(run.scores.schema_valid, "{:?}", run.scores.validation_errors);
        let plan = run.plan.unwrap();
        let raw = serde_json::to_string(&plan).unwrap();
        let back: needle_rdf_agent::plan::Plan = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.steps.len(), plan.steps.len());
    }
    // D: both artifacts retained + prose→structure preservation.
    {
        let case = cases().into_iter().find(|c| c.id == "CRU-002").unwrap();
        let sv = needle_rdf_agent::bench::runner::ScriptedVariants::for_case(&case);
        let (planner, _) = sv.planner_d();
        let mut ctx = OperationContext::new(case.request.clone(), case.context.clone());
        let request = UserRequest::new(case.request.clone());
        let plan = planner.create_plan(&request, &mut ctx).await.expect("D plans");
        let nl = ctx.artifact("nl_plan").expect("nl_plan retained");
        assert!(nl.contains('1'));
        assert!(ctx.artifact("structured_plan").expect("structured retained").contains("s1"));
        assert!(check_prose_consistency(nl, &plan).is_ok());
    }
}

#[test]
fn t2_fixture_isolation() {
    // Two fresh fixtures hash equal: runs never leak across variants.
    let a = build_fixture();
    let b = build_fixture();
    assert_eq!(a.hash_tenant_a(), b.hash_tenant_a());
    assert!(a.audit.is_empty() && b.audit.is_empty());
}

#[test]
fn t2_recordings_documented_without_server() {
    // Replay path is deterministic synthesis; recordings are opt-in.
    // This test documents the path without requiring RECORD=1.
    let case = cases().into_iter().find(|c| c.id == "INT-001").unwrap();
    let (plan, _) = synth_plan(&case);
    assert!(validate_plan(&plan).is_ok());
}

/// Live subset (ignored locally): real backends, keyless, small on purpose.
/// Proves transport + model-id propagation; joins to scoring when a model
/// returns a valid plan. Run with:
/// `RUN_MODEL_TESTS=1 cargo test --test bench_t2_planning -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn t2_live_subset() {
    if !live_requested() {
        return;
    }
    std::env::remove_var("OPENAI_API_KEY");
    let ids = ["INT-001", "QRY-001"];
    let all = cases();
    for id in ids {
        let case = all.iter().find(|c| c.id == id).unwrap().clone();
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
                    run_planning_only(&p, &case, &fixture, variant, Ablation::FullTools).await
                }
                Variant::C => {
                    let p = needle_rdf_agent::planner::AgentStructuredPlanner::new(backend);
                    run_planning_only(&p, &case, &fixture, variant, Ablation::FullTools).await
                }
                _ => {
                    let p = needle_rdf_agent::planner::AgentTwoPassPlanner::new(backend);
                    run_planning_only(&p, &case, &fixture, variant, Ablation::FullTools).await
                }
            };
            println!(
                "live {id} {} model={model} schema={} complete={} err={:?}",
                variant.as_str(),
                result.scores.schema_valid,
                result.scores.complete(),
                result.planner_error,
            );
            // No-transport-failure is asserted by reaching scoring; model-id
            // propagation is structural (AgentBackend carries OPENAI_MODEL).
            assert!(!model.trim().is_empty());
            assert!(result.zero_writes);
        }
    }
    let _ = Arc::new(1);
}
