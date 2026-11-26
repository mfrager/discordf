//! `create` tool: add a new resource via the guarded mutation path.

use super::{check_grounding, valid_iri, ToolError, ToolState};
use crate::policy::{execute_guarded, ExecutionContext, MutationOptions};
use crate::rdf::{GraphName, Quad, Term};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyInput {
    pub predicate: String,
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
}

impl PropertyInput {
    pub fn to_object(&self, tool: &str) -> Result<Term, ToolError> {
        match self.kind.as_str() {
            "iri" => {
                Term::iri(self.value.clone()).map_err(|e| ToolError::args(tool, e.to_string()))
            }
            "literal" => Ok(Term::literal(self.value.clone())),
            "typed" => {
                let dt = self
                    .datatype
                    .clone()
                    .ok_or_else(|| ToolError::args(tool, "typed value needs datatype"))?;
                Term::typed_literal(self.value.clone(), dt)
                    .map_err(|e| ToolError::args(tool, e.to_string()))
            }
            "lang" => {
                let lang = self
                    .lang
                    .clone()
                    .ok_or_else(|| ToolError::args(tool, "lang value needs lang"))?;
                Term::lang_literal(self.value.clone(), lang)
                    .map_err(|e| ToolError::args(tool, e.to_string()))
            }
            "bnode" => {
                Term::bnode(self.value.clone()).map_err(|e| ToolError::args(tool, e.to_string()))
            }
            other => Err(ToolError::args(tool, format!("unsupported kind: {other}"))),
        }
    }
}

pub fn property_to_quad(
    tool: &str,
    subject: &Term,
    graph: &GraphName,
    p: &PropertyInput,
) -> Result<Quad, ToolError> {
    if !valid_iri(&p.predicate) {
        return Err(ToolError::args(
            tool,
            format!("bad predicate IRI: {:?}", p.predicate),
        ));
    }
    let pred = Term::iri(p.predicate.clone()).map_err(|e| ToolError::args(tool, e.to_string()))?;
    let obj = p.to_object(tool)?;
    Quad::new(subject.clone(), pred, obj, graph.clone())
        .map_err(|e| ToolError::args(tool, e.to_string()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateArgs {
    #[serde(default)]
    pub uri: Option<String>,
    pub graph: String,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub properties: Vec<PropertyInput>,
}

pub fn handle(
    state: &ToolState,
    ctx: &ExecutionContext,
    args: CreateArgs,
) -> Result<serde_json::Value, ToolError> {
    let graph = GraphName::named(args.graph.clone())
        .map_err(|e| ToolError::args("create", e.to_string()))?;
    let uri = match args.uri {
        Some(u) => {
            if !valid_iri(&u) {
                return Err(ToolError::grounding(
                    "create",
                    format!("unsupported URI value: {u:?}"),
                ));
            }
            u
        }
        None => {
            let n = state
                .repo
                .len()
                .map_err(|e| ToolError::storage("create", e.to_string()))?;
            format!("urn:needle:gen:{}:{n}", ctx.request_id)
        }
    };
    for t in &args.types {
        if !valid_iri(t) {
            return Err(ToolError::args("create", format!("bad type IRI: {t:?}")));
        }
    }
    check_grounding("create", state, ctx, Some(&uri), &graph)?;
    let subject = Term::iri(uri.clone()).map_err(|e| ToolError::args("create", e.to_string()))?;
    let mut to_add = Vec::new();
    for t in &args.types {
        let ty = Term::iri(t.clone()).map_err(|e| ToolError::args("create", e.to_string()))?;
        to_add.push(
            Quad::new(
                subject.clone(),
                Term::iri(crate::ontology::RDF_TYPE).unwrap(),
                ty,
                graph.clone(),
            )
            .map_err(|e| ToolError::args("create", e.to_string()))?,
        );
    }
    for p in &args.properties {
        to_add.push(property_to_quad("create", &subject, &graph, p)?);
    }
    if to_add.is_empty() {
        return Err(ToolError::args(
            "create",
            "nothing to create (empty types+properties)",
        ));
    }
    let outcome = execute_guarded(
        &*state.repo,
        &state.reasoner,
        &state.shapes,
        &state.audit,
        ctx,
        &uri,
        &graph,
        &to_add,
        &[],
        MutationOptions::default(),
    )
    .map_err(|e| map_policy("create", e))?;
    let _ = state.search.sync_resource(&*state.repo, &uri, &graph);
    Ok(serde_json::json!({
        "uri": uri,
        "added": outcome.delta.added.len(),
        "summary": outcome.summary,
    }))
}

pub fn map_policy(tool: &str, e: crate::policy::PolicyError) -> ToolError {
    use crate::policy::PolicyError as P;
    match e {
        P::Denied(m) => ToolError::auth(tool, m),
        P::ValidationFailed(v) => ToolError::validation(
            tool,
            format!(
                "SHACL failed: {}",
                serde_json::to_value(&v).unwrap_or_default()
            ),
        ),
        P::ConfirmationRequired(m) => ToolError::confirmation(tool, m),
        P::Storage(m) => ToolError::storage(tool, m),
    }
}
