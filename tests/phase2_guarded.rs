//! Phase 2 acceptance: ontology reasoning, SHACL, authz, dry-run,
//! destructive gate, audit — all through the guarded mutation path.

use needle_rdf_agent::ontology::reasoner::{EX, RDF_TYPE};
use needle_rdf_agent::ontology::OntologyService;
use needle_rdf_agent::ontology::{
    contact_shapes, load_default_ontology, ontology_fixture_quads, OntologyReasoner,
};
use needle_rdf_agent::policy::{
    confirm, execute_guarded, AuditLog, CommitStatus, ExecutionContext, MutationOptions, Operation,
};
use needle_rdf_agent::rdf::{GraphName, OxigraphRepository, Quad, RdfRepository, Term};

fn setup() -> (OxigraphRepository, OntologyReasoner, AuditLog, GraphName) {
    let repo = OxigraphRepository::new_in_memory(5000).unwrap();
    load_default_ontology(&repo).unwrap();
    let reasoner = OntologyReasoner::from_repository(&repo).unwrap();
    let audit = AuditLog::in_memory();
    let graph = GraphName::tenant("acme").unwrap();
    (repo, reasoner, audit, graph)
}

fn ctx_full(req: &str) -> ExecutionContext {
    ExecutionContext::new(
        req,
        "alice",
        "acme",
        vec![GraphName::tenant("acme").unwrap(), GraphName::ontology()],
        vec![
            Operation::Read,
            Operation::Create,
            Operation::Update,
            Operation::Delete,
        ],
    )
}

fn contact_adds(graph: &GraphName, name: &str, with_name: bool) -> Vec<Quad> {
    let s = Term::iri(format!("{EX}{name}")).unwrap();
    let mut v = vec![Quad::new(
        s.clone(),
        Term::iri(RDF_TYPE).unwrap(),
        Term::iri(format!("{EX}Contact")).unwrap(),
        graph.clone(),
    )
    .unwrap()];
    if with_name {
        v.push(
            Quad::new(
                s,
                Term::iri(format!("{EX}name")).unwrap(),
                Term::literal("Alice"),
                graph.clone(),
            )
            .unwrap(),
        );
    }
    v
}

// --- Hierarchy / inverses (reasoned view, asserted view clean) ---

#[test]
fn hierarchy_closure_and_equivalence() {
    let r = OntologyReasoner::from_quads(&ontology_fixture_quads());
    assert!(r.is_subclass_of(&format!("{EX}Employee"), &format!("{EX}Person")));
    assert!(r.is_subclass_of(&format!("{EX}Employee"), &format!("{EX}Contact")));
    assert_eq!(
        r.equivalents(&format!("{EX}Person")),
        vec![format!("{EX}Human")]
    );
    assert_eq!(
        r.equivalents(&format!("{EX}Human")),
        vec![format!("{EX}Person")]
    );
}

#[test]
fn inverses_visible_in_reasoned_view_only() {
    let (repo, r, _, g) = setup();
    let employs = Term::iri(format!("{EX}employs")).unwrap();
    let acct = Term::iri(format!("{EX}Acme")).unwrap();
    let alice = Term::iri(format!("{EX}Alice")).unwrap();
    let q = Quad::new(acct.clone(), employs, alice.clone(), g.clone()).unwrap();
    let entailed = r.inverse_entailments(&q);
    assert_eq!(entailed.len(), 1);
    assert_eq!(
        entailed[0].predicate.as_iri().unwrap(),
        format!("{EX}employedBy")
    );
    // Asserted store unchanged by reasoning.
    assert_eq!(repo.len().unwrap(), ontology_fixture_quads().len());
}

// --- SHACL pass / fail + violation shape ---

#[test]
fn valid_contact_commits_and_audits() {
    let (repo, r, audit, g) = setup();
    let adds = contact_adds(&g, "Alice", true);
    let out = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("req-1"),
        &format!("{EX}Alice"),
        &g,
        &adds,
        &[],
        MutationOptions {
            dry_run: false,
            confirmation_token: None,
        },
    )
    .expect("valid contact must commit");
    assert!(!out.dry_run);
    assert_eq!(out.delta.added.len(), 2);
    assert!(repo.contains(&adds[0]).unwrap());
    let records = audit.list();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].commit_status, CommitStatus::Committed);
    assert_eq!(records[0].request_id, "req-1");
    assert_eq!(records[0].principal, "alice");
    assert_eq!(records[0].graph, g);
    assert_eq!(records[0].op, "create");
}

#[test]
fn missing_name_fails_with_exact_violation_fields() {
    let (repo, r, audit, g) = setup();
    let adds = contact_adds(&g, "Bob", false);
    let err = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("req-2"),
        &format!("{EX}Bob"),
        &g,
        &adds,
        &[],
        MutationOptions::default(),
    )
    .expect_err("must fail SHACL");
    let msg = format!("{err:?}");
    assert!(msg.contains("ValidationFailed"), "got {msg}");
    // Nothing committed.
    assert!(!repo.contains(&adds[0]).unwrap());
    let records = audit.list();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].commit_status, CommitStatus::ValidationFailed);
    assert_eq!(records[0].validation_violations, 1);
    // Violation payload has all five required fields (serialized).
    let v = match err {
        needle_rdf_agent::policy::PolicyError::ValidationFailed(v) => v,
        other => panic!("wrong error {other:?}"),
    };
    let json = serde_json::to_value(&v[0]).unwrap();
    for field in ["shape_id", "focus_node", "path", "severity", "message"] {
        assert!(json.get(field).is_some(), "missing {field} in {json}");
    }
    assert_eq!(v[0].shape_id, "ex:ContactShape");
    assert_eq!(v[0].path, format!("{EX}name"));
}

#[test]
fn account_range_must_be_account_instance() {
    let (repo, r, audit, g) = setup();
    // Eve is a Contact, not an Account.
    let eve = Term::iri(format!("{EX}Eve")).unwrap();
    repo.insert(&contact_adds(&g, "Eve", true)).unwrap();
    // Clear audit? Use fresh audit for this mutation.
    let audit2 = AuditLog::in_memory();
    let alice = Term::iri(format!("{EX}Alice")).unwrap();
    let adds = vec![
        Quad::new(
            alice.clone(),
            Term::iri(RDF_TYPE).unwrap(),
            Term::iri(format!("{EX}Contact")).unwrap(),
            g.clone(),
        )
        .unwrap(),
        Quad::new(
            alice.clone(),
            Term::iri(format!("{EX}name")).unwrap(),
            Term::literal("A"),
            g.clone(),
        )
        .unwrap(),
        Quad::new(
            alice,
            Term::iri(format!("{EX}account")).unwrap(),
            eve,
            g.clone(),
        )
        .unwrap(),
    ];
    let err = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit2,
        &ctx_full("req-3"),
        &format!("{EX}Alice"),
        &g,
        &adds,
        &[],
        MutationOptions::default(),
    )
    .expect_err("range violation expected");
    assert!(format!("{err:?}").contains("ValidationFailed"));
    let _ = audit;
}

// --- Authz matrix ---

#[test]
fn authz_matrix() {
    let (repo, r, audit, g) = setup();
    let other = GraphName::tenant("other").unwrap();
    let adds = contact_adds(&g, "Alice", true);
    let cases: Vec<(&str, ExecutionContext, GraphName, Operation, bool)> = vec![
        (
            "owner create allowed",
            ctx_full("m-1"),
            g.clone(),
            Operation::Create,
            true,
        ),
        (
            "cross-tenant denied",
            ExecutionContext::new(
                "m-2",
                "bob",
                "other",
                vec![other.clone()],
                vec![
                    Operation::Create,
                    Operation::Update,
                    Operation::Delete,
                    Operation::Read,
                ],
            ),
            g.clone(),
            Operation::Create,
            false,
        ),
        (
            "op not permitted denied",
            ExecutionContext::new(
                "m-3",
                "alice",
                "acme",
                vec![g.clone()],
                vec![Operation::Read],
            ),
            g.clone(),
            Operation::Create,
            false,
        ),
        (
            "graph not allowlisted denied",
            ExecutionContext::new(
                "m-4",
                "alice",
                "acme",
                vec![GraphName::tenant("elsewhere").unwrap()],
                vec![Operation::Create],
            ),
            g.clone(),
            Operation::Create,
            false,
        ),
    ];
    for (label, ctx, graph, op, allowed) in cases {
        let res = needle_rdf_agent::policy::authz::authorize(&ctx, &graph, op);
        assert_eq!(res.is_ok(), allowed, "{label}");
    }
    // End-to-end: cross-tenant principal mutating acme graph is denied + audited.
    let evil = ExecutionContext::new(
        "evil-1",
        "mallory",
        "other",
        vec![other],
        vec![Operation::Create, Operation::Update, Operation::Delete],
    );
    let before = audit.len();
    let err = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &evil,
        &format!("{EX}Alice"),
        &g,
        &adds,
        &[],
        MutationOptions::default(),
    )
    .expect_err("cross-tenant must be denied");
    assert!(format!("{err:?}").contains("Denied"));
    assert_eq!(audit.len(), before + 1);
    assert_eq!(
        audit.list().last().unwrap().commit_status,
        CommitStatus::Denied
    );
}

// --- Dry-run determinism ---

#[test]
fn dry_run_changes_nothing_and_is_deterministic() {
    let (repo, r, audit, g) = setup();
    let adds = contact_adds(&g, "Alice", true);
    let opts = MutationOptions {
        dry_run: true,
        confirmation_token: None,
    };
    let o1 = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("d-1"),
        &format!("{EX}Alice"),
        &g,
        &adds,
        &[],
        opts.clone(),
    )
    .unwrap();
    let o2 = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("d-2"),
        &format!("{EX}Alice"),
        &g,
        &adds,
        &[],
        opts,
    )
    .unwrap();
    assert!(o1.dry_run && o2.dry_run);
    assert_eq!(o1.delta, o2.delta, "dry-runs must be identical");
    assert!(
        repo.is_empty().unwrap()
            || repo
                .match_quads(None, None, None, Some(&g), None)
                .unwrap()
                .is_empty()
    );
    // Both dry-runs audited.
    assert!(
        audit
            .list()
            .iter()
            .filter(|a| a.commit_status == CommitStatus::DryRun)
            .count()
            >= 2
    );
}

// --- Destructive gate ---

#[test]
fn delete_without_token_rejected_with_token_proceeds() {
    let (repo, r, audit, g) = setup();
    let adds = contact_adds(&g, "Alice", true);
    execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("c-1"),
        &format!("{EX}Alice"),
        &g,
        &adds,
        &[],
        MutationOptions::default(),
    )
    .unwrap();
    // Without token → ConfirmationRequired, triples intact.
    let err = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("c-2"),
        &format!("{EX}Alice"),
        &g,
        &[],
        &adds,
        MutationOptions::default(),
    )
    .expect_err("delete needs confirmation");
    assert!(format!("{err:?}").contains("ConfirmationRequired"));
    assert!(repo.contains(&adds[0]).unwrap());
    // With token → proceeds.
    let out = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("c-3"),
        &format!("{EX}Alice"),
        &g,
        &[],
        &adds,
        MutationOptions {
            dry_run: false,
            confirmation_token: Some(format!("{}c-3", confirm::REQUIRED_CONFIRMATION_PREFIX)),
        },
    )
    .unwrap();
    assert_eq!(out.delta.removed.len(), 2);
    assert!(!repo.contains(&adds[0]).unwrap());
}

// --- Audit: every path appends exactly one record; survives restart ---

#[test]
fn audit_records_every_outcome_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let repo = OxigraphRepository::new_in_memory(5000).unwrap();
    load_default_ontology(&repo).unwrap();
    let r = OntologyReasoner::from_repository(&repo).unwrap();
    let audit = AuditLog::open(&audit_path).unwrap();
    let g = GraphName::tenant("acme").unwrap();

    // Commit.
    execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("a-1"),
        &format!("{EX}Alice"),
        &g,
        &contact_adds(&g, "Alice", true),
        &[],
        MutationOptions::default(),
    )
    .unwrap();
    // Dry-run.
    execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("a-2"),
        &format!("{EX}Bob"),
        &g,
        &contact_adds(&g, "Bob", true),
        &[],
        MutationOptions {
            dry_run: true,
            confirmation_token: None,
        },
    )
    .unwrap();
    // Validation-failed.
    let _ = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("a-3"),
        &format!("{EX}NoName"),
        &g,
        &contact_adds(&g, "NoName", false),
        &[],
        MutationOptions::default(),
    );
    // Denied (no perms).
    let denied_ctx = ExecutionContext::new("a-4", "nobody", "acme", vec![], vec![]);
    let _ = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &denied_ctx,
        &format!("{EX}Alice"),
        &g,
        &contact_adds(&g, "Alice", true),
        &[],
        MutationOptions::default(),
    );
    assert_eq!(audit.len(), 4);

    let statuses: Vec<CommitStatus> = audit.list().iter().map(|a| a.commit_status).collect();
    assert_eq!(
        statuses,
        vec![
            CommitStatus::Committed,
            CommitStatus::DryRun,
            CommitStatus::ValidationFailed,
            CommitStatus::Denied
        ]
    );

    // Survives restart.
    drop(audit);
    let reopened = AuditLog::open(&audit_path).unwrap();
    assert_eq!(reopened.len(), 4);
}

// --- Property: random resources never panic, always typed result ---

#[test]
fn random_resources_never_panic() {
    let (repo, r, audit, g) = setup();
    let mut x: u64 = 0x12345678;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for i in 0..50 {
        let name = format!("R{}", next() % 1000);
        let with_name = next() % 2 == 0;
        let adds = contact_adds(&g, &name, with_name);
        let res = execute_guarded(
            &repo,
            &r,
            &contact_shapes(),
            &audit,
            &ctx_full(&format!("prop-{i}")),
            &format!("{EX}{name}"),
            &g,
            &adds,
            &[],
            MutationOptions::default(),
        );
        // Must be a typed result, never a panic; conformance tracks with_name.
        match res {
            Ok(_) => assert!(with_name, "{name} committed without name"),
            Err(e) => assert!(
                format!("{e:?}").contains("ValidationFailed"),
                "{name}: {e:?}"
            ),
        }
    }
}

// --- Snapshot: golden violation + audit JSON ---

#[test]
fn snapshot_violation_and_audit_shape() {
    let (repo, r, audit, g) = setup();
    let adds = contact_adds(&g, "Bob", false);
    let err = execute_guarded(
        &repo,
        &r,
        &contact_shapes(),
        &audit,
        &ctx_full("s-1"),
        &format!("{EX}Bob"),
        &g,
        &adds,
        &[],
        MutationOptions::default(),
    )
    .unwrap_err();
    let violations = match err {
        needle_rdf_agent::policy::PolicyError::ValidationFailed(v) => v,
        other => panic!("{other:?}"),
    };
    let v_json = serde_json::to_string_pretty(&violations).unwrap();
    // Golden fields — fail on schema drift.
    assert!(v_json.contains("ex:ContactShape"));
    assert!(v_json.contains("focus_node"));
    let rec = audit.list().pop().unwrap();
    let rec_json = serde_json::to_value(&rec).unwrap();
    for field in [
        "request_id",
        "principal",
        "target_uri",
        "graph",
        "op",
        "delta",
        "validation_conforms",
        "validation_violations",
        "commit_status",
    ] {
        assert!(rec_json.get(field).is_some(), "audit missing {field}");
    }
    // Ensure inferred triples were never persisted as asserted.
    let total: usize = repo.len().unwrap();
    assert_eq!(
        total,
        ontology_fixture_quads().len(),
        "failed validation must not write anything"
    );
}
