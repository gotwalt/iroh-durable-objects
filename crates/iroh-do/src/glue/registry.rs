//! The endpoint registry object (design note 009), which the `registry` Durable Object class
//! that [`worker!`](crate::worker) declares delegates to, and the calls other code makes to it.
//!
//! One instance, named [`REGISTRY_NAME`], records every minted EndpointId so that the router
//! can list them. Endpoint objects register themselves at mint, or on their first wake if they
//! were minted before the registry existed.

use worker::{
    Env, Headers, Method, Request, RequestInit, Response, Result, SqlStorage, SqlStorageValue,
    State,
};

use crate::registry::{
    Entry, LIST, LIST_LIMIT, MAX_LABEL, REGISTER, REGISTRY_NAME, SCHEMA, UNREGISTER,
};

/// Durable Object namespace binding of the registry.
pub const REGISTRY_BINDING: &str = "REGISTRY";
/// Internal path: record an endpoint (JSON [`Entry`]).
const INTERNAL_REGISTER: &str = "/_internal/register";
/// Internal path: list endpoints, newest first.
const INTERNAL_LIST: &str = "/_internal/list";
/// Internal path: remove an endpoint (its z-base-32 id is the request body).
const INTERNAL_UNREGISTER: &str = "/_internal/unregister";

/// The endpoint registry.
pub struct Registry {
    sql: SqlStorage,
}

impl Registry {
    /// The class constructor.
    pub fn new(state: State, env: Env) -> Self {
        super::logging::install(&env);
        let sql = state.storage().sql();
        for stmt in SCHEMA {
            if let Err(e) = sql.exec(stmt, None) {
                tracing::error!(event = "registry_schema_failed", error = %e);
            }
        }
        Self { sql }
    }

    /// The class's `fetch`: the internal register, unregister and list calls.
    pub async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, INTERNAL_REGISTER) => {
                let e: Entry = req.json().await?;
                if iroh_base::PublicKey::from_z32(&e.id).is_err()
                    || e.label.chars().count() > MAX_LABEL
                {
                    return Response::error("bad entry", 400);
                }
                let entry = e.id.clone();
                self.sql.exec(
                    REGISTER,
                    vec![
                        e.id.into(),
                        e.app.into(),
                        e.label.into(),
                        SqlStorageValue::Integer(e.minted_at),
                    ],
                )?;
                tracing::info!(event = "registry_entry", endpoint = %entry);
                Response::empty()
            }
            (Method::Post, INTERNAL_UNREGISTER) => {
                let id = req.text().await?;
                if iroh_base::PublicKey::from_z32(&id).is_err() {
                    return Response::error("bad id", 400);
                }
                self.sql.exec(UNREGISTER, vec![id.clone().into()])?;
                tracing::info!(event = "registry_remove", endpoint = %id);
                Response::empty()
            }
            (Method::Get, INTERNAL_LIST) => {
                let rows: Vec<Entry> = self
                    .sql
                    .exec(LIST, vec![SqlStorageValue::Integer(LIST_LIMIT as i64)])?
                    .to_array()?;
                Response::from_json(&rows)
            }
            _ => Response::error("not found", 404),
        }
    }
}

fn stub(env: &Env) -> Result<worker::Stub> {
    env.durable_object(REGISTRY_BINDING)?
        .id_from_name(REGISTRY_NAME)?
        .get_stub()
}

/// Records `entry` in the registry.
pub async fn register(env: &Env, entry: &Entry) -> Result<()> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(entry)?.into()));
    let req = Request::new_with_init(&format!("https://do{INTERNAL_REGISTER}"), &init)?;
    let resp = stub(env)?.fetch_with_request(req).await?;
    if resp.status_code() / 100 != 2 {
        return Err(worker::Error::RustError(format!(
            "registry answered {}",
            resp.status_code()
        )));
    }
    Ok(())
}

/// Removes `id` (z-base-32) from the registry. Removing an absent id succeeds.
pub async fn unregister(env: &Env, id: &str) -> Result<()> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(id.to_string().into()));
    let req = Request::new_with_init(&format!("https://do{INTERNAL_UNREGISTER}"), &init)?;
    let resp = stub(env)?.fetch_with_request(req).await?;
    if resp.status_code() / 100 != 2 {
        return Err(worker::Error::RustError(format!(
            "registry answered {}",
            resp.status_code()
        )));
    }
    Ok(())
}

/// Returns the newest registered endpoints.
pub async fn list(env: &Env) -> Result<Vec<Entry>> {
    let mut resp = stub(env)?
        .fetch_with_str(&format!("https://do{INTERNAL_LIST}"))
        .await?;
    resp.json().await
}
