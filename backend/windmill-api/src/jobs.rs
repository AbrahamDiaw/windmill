/*
 * Author: Ruben Fiszel
 * Copyright: Windmill Labs, Inc 2022
 * This file and its contents are licensed under the AGPLv3 License.
 * Please see the included NOTICE for copyright information and
 * LICENSE-AGPL for a copy of the license.
 */

use axum::body::Body;
use axum::http::HeaderValue;
use futures::TryFutureExt;
use http::{HeaderMap, HeaderName};
use itertools::Itertools;
use quick_cache::sync::Cache;
use serde_json::value::RawValue;
use sqlx::Pool;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
#[cfg(feature = "prometheus")]
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;
#[cfg(feature = "prometheus")]
use tokio::time::Instant;
use tower::ServiceBuilder;
use windmill_common::error::JsonResult;
use windmill_common::flow_status::{JobResult, RestartedFrom};
use windmill_common::jobs::{format_completed_job_result, format_result, ENTRYPOINT_OVERRIDE};
use windmill_common::worker::{CLOUD_HOSTED, TMP_DIR};

#[cfg(all(feature = "enterprise", feature = "parquet"))]
use windmill_common::scripts::PREVIEW_IS_CODEBASE_HASH;
use windmill_common::variables::get_workspace_key;

use crate::add_webhook_allowed_origin;
use crate::concurrency_groups::join_concurrency_key;
use crate::db::ApiAuthed;

use crate::users::get_scope_tags;
use crate::utils::content_plain;
use crate::{
    args::{DecodeQueries, WebhookArgs},
    db::DB,
    users::{check_scopes, require_owner_of_path, OptAuthed},
    utils::require_super_admin,
};
use anyhow::Context;
use axum::{
    extract::{FromRequest, Json, Path, Query},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Router,
};
use base64::Engine;
use chrono::Utc;
use hmac::Mac;
use hyper::{Request, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sql_builder::prelude::*;
use sqlx::types::JsonRawValue;
use sqlx::{types::Uuid, FromRow, Postgres, Transaction};
use tower_http::cors::{Any, CorsLayer};
use urlencoding::encode;
use windmill_audit::audit_ee::{audit_log, AuditAuthor};
use windmill_audit::ActionKind;
use windmill_common::worker::{to_raw_value, CUSTOM_TAGS_PER_WORKSPACE};
use windmill_common::{
    cache,
    db::UserDB,
    error::{self, to_anyhow, Error},
    flow_status::{Approval, FlowStatus, FlowStatusModule},
    flows::{add_virtual_items_if_necessary, resolve_maybe_value, FlowValue},
    jobs::{script_path_to_payload, CompletedJob, JobKind, JobPayload, QueuedJob, RawCode},
    oauth2::HmacSha256,
    scripts::{ScriptHash, ScriptLang},
    users::username_to_permissioned_as,
    utils::{
        not_found_if_none, now_from_db, paginate, paginate_without_limits, require_admin,
        Pagination, StripPath,
    },
};

#[cfg(all(feature = "enterprise", feature = "parquet"))]
use windmill_common::s3_helpers::OBJECT_STORE_CACHE_SETTINGS;
#[cfg(feature = "prometheus")]
use windmill_common::{METRICS_DEBUG_ENABLED, METRICS_ENABLED};

use windmill_common::{get_latest_deployed_hash_for_path, BASE_URL};
use windmill_queue::{
    cancel_job, get_queued_job, get_result_and_success_by_id_from_flow, job_is_complete, push,
    PushArgs, PushArgsOwned, PushIsolationLevel,
};

#[cfg(feature = "prometheus")]
type Histo = prometheus::Histogram;

#[cfg(not(feature = "prometheus"))]
type Histo = ();

#[cfg(feature = "prometheus")]
fn setup_list_jobs_debug_metrics() -> Option<Histo> {
    let api_list_jobs_query_duration = if METRICS_DEBUG_ENABLED.load(Ordering::Relaxed)
        && METRICS_ENABLED.load(Ordering::Relaxed)
    {
        Some(
            prometheus::register_histogram!(prometheus::HistogramOpts::new(
                "api_list_jobs_query_duration",
                "Duration of listing jobs (query)",
            ))
            .expect("register prometheus metric"),
        )
    } else {
        None
    };

    api_list_jobs_query_duration
}

#[cfg(not(feature = "prometheus"))]
fn setup_list_jobs_debug_metrics() -> Option<Histo> {
    None
}

pub fn workspaced_service() -> Router {
    let cors = CorsLayer::new()
        .allow_methods([http::Method::GET, http::Method::POST])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
        .allow_origin(Any);

    // Cloud events abuse control headers
    let ce_headers =
        ServiceBuilder::new().layer(axum::middleware::from_fn(add_webhook_allowed_origin));

    let api_list_jobs_query_duration = setup_list_jobs_debug_metrics();

    Router::new()
        .route(
            "/run/f/*script_path",
            post(run_flow_by_path)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route(
            "/run/workflow_as_code/:job_id/:entrypoint",
            post(run_workflow_as_code)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route(
            "/restart/f/:job_id/from/:step_id",
            post(restart_flow).head(|| async { "" }).layer(cors.clone()),
        )
        .route(
            "/restart/f/:job_id/from/:step_id/:branch_of_iteration_n",
            post(restart_flow).head(|| async { "" }).layer(cors.clone()),
        )
        .route(
            "/run/p/*script_path",
            post(run_script_by_path)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route(
            "/run_wait_result/p/*script_path",
            post(run_wait_result_script_by_path)
                .get(run_wait_result_job_by_path_get)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route(
            "/run_wait_result/h/:hash",
            post(run_wait_result_script_by_hash)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route(
            "/run_wait_result/f/*script_path",
            post(run_wait_result_flow_by_path)
                .get(run_wait_result_flow_by_path_get)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route(
            "/run/h/:hash",
            post(run_job_by_hash)
                .head(|| async { "" })
                .layer(cors.clone())
                .layer(ce_headers.clone()),
        )
        .route("/run/preview", post(run_preview_script))
        .route(
            "/run/preview_bundle",
            post(run_bundle_preview_script).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/add_batch_jobs/:n", post(add_batch_jobs))
        .route("/run/preview_flow", post(run_preview_flow_job))
        .route(
            "/list",
            get(list_jobs).layer(Extension(api_list_jobs_query_duration)),
        )
        .route("/queue/list", get(list_queue_jobs))
        .route("/queue/count", get(count_queue_jobs))
        .route("/queue/list_filtered_uuids", get(list_filtered_uuids))
        .route("/queue/cancel_selection", post(cancel_selection))
        .route("/completed/count", get(count_completed_jobs))
        .route("/completed/count_jobs", get(count_completed_jobs_detail))
        .route(
            "/completed/list",
            get(list_completed_jobs).layer(cors.clone()),
        )
        .route(
            "/completed/get/:id",
            get(get_completed_job).layer(cors.clone()),
        )
        .route(
            "/completed/get_result/:id",
            get(get_completed_job_result).layer(cors.clone()),
        )
        .route(
            "/completed/get_result_maybe/:id",
            get(get_completed_job_result_maybe).layer(cors.clone()),
        )
        .route(
            "/completed/delete/:id",
            post(delete_completed_job).layer(cors.clone()),
        )
        .route(
            "/flow/resume/:id",
            post(resume_suspended_flow_as_owner).layer(cors.clone()),
        )
        .route(
            "/job_signature/:job_id/:resume_id",
            get(create_job_signature).layer(cors.clone()),
        )
        .route(
            "/flow/user_states/:job_id/:key",
            get(get_flow_user_state)
                .post(set_flow_user_state)
                .layer(cors.clone()),
        )
        .route(
            "/resume_urls/:job_id/:resume_id",
            get(get_resume_urls).layer(cors.clone()),
        )
        .route(
            "/result_by_id/:job_id/:node_id",
            get(get_result_by_id).layer(cors.clone()),
        )
        .route("/run/dependencies", post(run_dependencies_job))
        .route("/run/flow_dependencies", post(run_flow_dependencies_job))
}

pub fn workspace_unauthed_service() -> Router {
    Router::new()
        .route(
            "/resume/:job_id/:resume_id/:secret",
            get(resume_suspended_job),
        )
        .route(
            "/resume/:job_id/:resume_id/:secret",
            post(resume_suspended_job),
        )
        .route(
            "/cancel/:job_id/:resume_id/:secret",
            get(cancel_suspended_job),
        )
        .route(
            "/cancel/:job_id/:resume_id/:secret",
            post(cancel_suspended_job),
        )
        .route(
            "/get_flow/:job_id/:resume_id/:secret",
            get(get_suspended_job_flow),
        )
        .route("/get_root_job_id/:id", get(get_root_job))
        .route("/get/:id", get(get_job))
        .route("/get_logs/:id", get(get_job_logs))
        .route("/get_args/:id", get(get_args))
        .route("/get_flow_debug_info/:id", get(get_flow_job_debug_info))
        .route("/completed/get/:id", get(get_completed_job))
        .route("/completed/get_result/:id", get(get_completed_job_result))
        .route(
            "/completed/get_result_maybe/:id",
            get(get_completed_job_result_maybe),
        )
        .route("/getupdate/:id", get(get_job_update))
        .route("/get_log_file/*file_path", get(get_log_file))
        .route("/queue/cancel/:id", post(cancel_job_api))
        .route(
            "/queue/cancel_persistent/*script_path",
            post(cancel_persistent_script_api),
        )
        .route("/queue/force_cancel/:id", post(force_cancel))
}

pub fn global_root_service() -> Router {
    Router::new()
        .route("/db_clock", get(get_db_clock))
        .route("/completed/count_by_tag", get(count_by_tag))
}

#[derive(Deserialize)]
struct JsonPath {
    pub json_path: Option<String>,
    pub suspended_job: Option<Uuid>,
    pub resume_id: Option<u32>,
    pub secret: Option<String>,
    pub approver: Option<String>,
}
async fn get_result_by_id(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, flow_id, node_id)): Path<(String, Uuid, String)>,
    Query(JsonPath { json_path, .. }): Query<JsonPath>,
) -> windmill_common::error::JsonResult<Box<JsonRawValue>> {
    let res =
        windmill_queue::get_result_by_id(db.clone(), w_id.clone(), flow_id, node_id, json_path)
            .await?;

    log_job_view(&db, Some(&authed), &w_id, &flow_id).await?;

    Ok(Json(res))
}

async fn get_root_job(
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
) -> windmill_common::error::JsonResult<String> {
    let res = compute_root_job_for_flow(&db, &w_id, id).await?;
    Ok(Json(res))
}

async fn compute_root_job_for_flow(db: &DB, w_id: &str, job_id: Uuid) -> error::Result<String> {
    let mut job = get_queued_job(&job_id, w_id, db).await?;
    while let Some(j) = job {
        if let Some(uuid) = j.parent_job {
            job = get_queued_job(&uuid, w_id, db).await?;
        } else {
            return Ok(j.id.to_string());
        }
    }
    Ok(job_id.to_string())
}

async fn get_db_clock(Extension(db): Extension<DB>) -> windmill_common::error::JsonResult<i64> {
    Ok(Json(now_from_db(&db).await?.timestamp_millis()))
}

async fn cancel_job_api(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
    Json(CancelJob { reason }): Json<CancelJob>,
) -> error::Result<String> {
    let tx = db.begin().await?;

    let audit_author: AuditAuthor = match opt_authed.as_ref() {
        Some(authed) => (authed).into(),
        None => AuditAuthor {
            username: "anonymous".to_string(),
            username_override: None,
            email: "anonymous".to_string(),
        },
    };

    let (mut tx, job_option) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        windmill_queue::cancel_job(
            &audit_author.username,
            reason,
            id,
            &w_id,
            tx,
            &db,
            false,
            opt_authed.is_none(),
        ),
    )
    .await
    .map_err(|e| {
        Error::InternalErr(format!(
            "timeout after 120s while cancelling job {id} in {w_id}: {e:#}"
        ))
    })??;

    if let Some(id) = job_option {
        audit_log(
            &mut *tx,
            &audit_author,
            "jobs.cancel",
            ActionKind::Delete,
            &w_id,
            Some(&id.to_string()),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(id.to_string())
    } else {
        tx.commit().await?;
        if job_is_complete(&db, id, &w_id).await.unwrap_or(false) {
            return Ok(format!("queued job id {} is already completed", id));
        } else {
            return Err(error::Error::NotFound(format!(
                "queued job id {} does not exist",
                id
            )));
        }
    }
}

async fn cancel_persistent_script_api(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, script_path)): Path<(String, StripPath)>,
    Json(CancelJob { reason }): Json<CancelJob>,
) -> error::Result<()> {
    let audit_author: AuditAuthor = match opt_authed {
        Some(authed) => (&authed).into(),
        None => {
            return Err(Error::BadRequest(format!(
                "Cancelling persistent script require to be logged in and member of {w_id}"
            )))
        }
    };

    let cancelled_job_ids = windmill_queue::cancel_persistent_script_jobs(
        &audit_author.username,
        reason,
        script_path.to_path(),
        &w_id,
        &db,
    )
    .await?;

    audit_log(
        &db,
        &audit_author,
        "jobs.cancel_persistent",
        ActionKind::Delete,
        &w_id,
        Some(script_path.to_path()),
        Some(
            [(
                "job_ids",
                cancelled_job_ids
                    .into_iter()
                    .map(|uuid: Uuid| uuid.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
                    .as_str(),
            )]
            .into(),
        ),
    )
    .await?;
    Ok(())
}

async fn force_cancel(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
    Json(CancelJob { reason }): Json<CancelJob>,
) -> error::Result<String> {
    let tx = db.begin().await?;

    let audit_author: AuditAuthor = match opt_authed.as_ref() {
        Some(authed) => (authed).into(),
        None => AuditAuthor {
            username: "anonymous".to_string(),
            username_override: None,
            email: "anonymous".to_string(),
        },
    };

    let (mut tx, job_option) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        windmill_queue::cancel_job(
            &audit_author.username,
            reason,
            id,
            &w_id,
            tx,
            &db,
            true,
            opt_authed.is_none(),
        ),
    )
    .await
    .map_err(|e| {
        Error::InternalErr(format!(
            "timeout after 120s while cancelling job {id} in {w_id}: {e:#}"
        ))
    })??;

    if let Some(id) = job_option {
        audit_log(
            &mut *tx,
            &audit_author,
            "jobs.force_cancel",
            ActionKind::Delete,
            &w_id,
            Some(&id.to_string()),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(id.to_string())
    } else {
        tx.commit().await?;
        if job_is_complete(&db, id, &w_id).await.unwrap_or(false) {
            return Ok(format!("queued job id {} is already completed", id));
        } else {
            return Err(error::Error::NotFound(format!(
                "queued job id {} does not exist",
                id
            )));
        }
    }
}

pub async fn get_path_tag_limits_cache_for_hash(
    mut tx: Transaction<'_, Postgres>,
    w_id: &str,
    hash: i64,
) -> error::Result<(
    String,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    ScriptLang,
    Option<bool>,
    Option<i16>,
    Option<bool>,
    Option<i32>,
    Option<bool>,
    Option<String>,
    String,
)> {
    let script = sqlx::query!(
        "select path, tag, concurrency_key, concurrent_limit, concurrency_time_window_s, cache_ttl, language as \"language: ScriptLang\", dedicated_worker, priority, delete_after_use, timeout, has_preprocessor, on_behalf_of_email, created_by from script where hash = $1 AND workspace_id = $2",
        hash,
        w_id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        Error::InternalErr(format!(
            "querying getting path for hash {hash} in {w_id}: {e:#}"
        ))
    })?.ok_or_else(|| Error::NotFound(format!(
        "deployed script not found at hash {hash} in workspace {w_id}"
    )))?;
    Ok((
        script.path,
        script.tag,
        script.concurrency_key,
        script.concurrent_limit,
        script.concurrency_time_window_s,
        script.cache_ttl,
        script.language,
        script.dedicated_worker,
        script.priority,
        script.delete_after_use,
        script.timeout,
        script.has_preprocessor,
        script.on_behalf_of_email,
        script.created_by,
    ))
}

async fn get_flow_job_debug_info(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
) -> error::Result<Response> {
    let job = GetQuery::new().fetch_queued(&db, id, &w_id).await?;
    if let Some(job) = job {
        let is_flow = job.is_flow();
        if job.is_flow_step || !is_flow {
            return Err(error::Error::BadRequest(
                "This endpoint is only for root flow jobs".to_string(),
            ));
        }
        let leaf_jobs: HashMap<String, JobResult> = job
            .leaf_jobs
            .clone()
            .and_then(|x| serde_json::from_value(x).ok())
            .unwrap_or_else(HashMap::new);
        let mut jobs = HashMap::new();
        let id = job.id.clone();
        jobs.insert("root_job".to_string(), Job::QueuedJob(job));

        let mut job_ids = vec![];
        let jobs_with_root = sqlx::query_scalar!(
            "SELECT id FROM queue WHERE workspace_id = $1 and root_job = $2",
            &w_id,
            &id,
        )
        .fetch_all(&db)
        .await?;

        for job in jobs_with_root {
            job_ids.push(job);
        }

        for job in leaf_jobs.iter() {
            match job.1 {
                JobResult::ListJob(jobs) => job_ids.extend(jobs.to_owned()),
                JobResult::SingleJob(job) => job_ids.push(job.clone()),
            }
        }
        for job_id in job_ids {
            let job = GetQuery::new()
                .with_auth(&opt_authed)
                .fetch(&db, job_id, &w_id)
                .await;
            if let Ok(job) = job {
                jobs.insert(job.id().to_string(), job);
            }
        }

        log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

        Ok(Json(jobs).into_response())
    } else {
        Err(error::Error::NotFound(format!(
            "QueuedJob {} not found",
            id
        )))
    }
}

#[derive(Deserialize)]
struct GetJobQuery {
    pub no_logs: Option<bool>,
}

async fn get_job(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
    Query(GetJobQuery { no_logs }): Query<GetJobQuery>,
) -> error::Result<Response> {
    let tags = opt_authed
        .as_ref()
        .map(|authed| get_scope_tags(authed))
        .flatten();

    let mut get = GetQuery::new()
        .with_auth(&opt_authed)
        .with_in_tags(tags.as_ref());

    if no_logs.unwrap_or(false) {
        get = get.without_logs();
    }
    let mut job = get.fetch(&db, id, &w_id).await?;
    job.fetch_outstanding_wait_time(&db).await?;

    log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

    Ok(Json(job).into_response())
}

macro_rules! get_job_query {
    ("completed_job_view", $($opts:tt)*) => {
        get_job_query!(
            @impl "completed_job_view", ($($opts)*),
            "duration_ms, success, result, deleted, is_skipped, result->'wm_labels' as labels, \
            CASE WHEN result is null or pg_column_size(result) < 90000 THEN result ELSE '\"WINDMILL_TOO_BIG\"'::jsonb END as result",
        )
    };
    ("queue_view", $($opts:tt)*) => {
        get_job_query!(
            @impl "queue_view", ($($opts)*),
            "scheduled_for, running, last_ping, suspend, suspend_until, same_worker, pre_run_error, visible_to_owner, \
            root_job, leaf_jobs, concurrent_limit, concurrency_time_window_s, timeout, flow_step_id, cache_ttl",
        )
    };
    (@impl $table:literal, (with_logs: $with_logs:expr, $($rest:tt)*), $additional_fields:literal, $($args:tt)*) => {
        if $with_logs {
            get_job_query!(@impl $table, ($($rest)*), $additional_fields, logs = const_format::formatcp!("right({}.logs, 20000)", $table), $($args)*)
        } else {
            get_job_query!(@impl $table, ($($rest)*), $additional_fields, logs = "null", $($args)*)
        }
    };
    (@impl $table:literal, (with_code: $with_code:expr, $($rest:tt)*), $additional_fields:literal, $($args:tt)*) => {
        if $with_code {
            get_job_query!(@impl $table, ($($rest)*), $additional_fields, lock = "raw_lock", code = "raw_code", $($args)*)
        } else {
            get_job_query!(@impl $table, ($($rest)*), $additional_fields, lock = "null", code = "null", $($args)*)
        }
    };
    (@impl $table:literal, (with_flow: $with_flow:expr, $($rest:tt)*), $additional_fields:literal, $($args:tt)*) => {
        if $with_flow {
            get_job_query!(@impl $table, ($($rest)*), $additional_fields, flow = "raw_flow", $($args)*)
        } else {
            get_job_query!(@impl $table, ($($rest)*), $additional_fields, flow = "null", $($args)*)
        }
    };
    (@impl $table:literal, (), $additional_fields:literal, $($args:tt)*) => {
        const_format::formatcp!(
            "SELECT \
            id, {table}.workspace_id, parent_job, created_by, {table}.created_at, started_at, script_hash, script_path, \
            CASE WHEN args is null or pg_column_size(args) < 90000 THEN args ELSE '{{\"reason\": \"WINDMILL_TOO_BIG\"}}'::jsonb END as args, \
            {logs} as logs, {code} as raw_code, canceled, canceled_by, canceled_reason, job_kind, \
            schedule_path, permissioned_as, flow_status, {flow} as raw_flow, is_flow_step, language, \
            {lock} as raw_lock, email, visible_to_owner, mem_peak, tag, priority, {additional_fields} \
            FROM {table} \
            WHERE id = $1 AND {table}.workspace_id = $2 AND ($3::text[] IS NULL OR tag = ANY($3)) LIMIT 1",
            table = $table,
            additional_fields = $additional_fields,
            $($args)*
        )
    }
}

#[derive(Copy, Clone)]
struct GetQuery<'a> {
    with_logs: bool,
    with_code: bool,
    with_flow: bool,
    with_auth: Option<&'a Option<ApiAuthed>>,
    with_in_tags: Option<&'a Vec<&'a str>>,
}

impl<'a> GetQuery<'a> {
    fn new() -> Self {
        Self {
            with_logs: true,
            with_code: true,
            with_flow: true,
            with_auth: None,
            with_in_tags: None,
        }
    }

    fn without_logs(self) -> Self {
        Self { with_logs: false, ..self }
    }

    fn without_code(self) -> Self {
        Self { with_code: false, ..self }
    }

    fn without_flow(self) -> Self {
        Self { with_flow: false, ..self }
    }

    fn with_auth(self, auth: &'a Option<ApiAuthed>) -> Self {
        Self { with_auth: Some(auth), ..self }
    }

    fn with_in_tags(self, in_tags: Option<&'a Vec<&str>>) -> Self {
        Self { with_in_tags: in_tags, ..self }
    }

    fn check_auth(&self, email: Option<&str>) -> error::Result<()> {
        if let Some(email) = email {
            if self.with_auth.is_some_and(|x| x.is_none()) && email != "anonymous" {
                return Err(Error::BadRequest(
                    "As a non logged in user, you can only see jobs ran by anonymous users"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Resolve job raw values.
    /// This fetch the raw values from the cache and update the job accordingly.
    ///
    /// # Details
    /// Most of the raw values (code, lock and flow) had been removed from the `job`, `queue` and
    /// `completed_job` tables. Only remains ones for "preview" jobs (i.e. [`JobKind::Preview`],
    /// [`JobKind::FlowPreview`] and [`JobKind::Dependencies`]). [`JobKind::Flow`] as well but only
    /// when pushed from an un-updated workers.
    /// This function is used to make the above change transparent for the API, as the returned jobs
    /// will have the raw values as if they were still in the tables.
    async fn resolve_raw_values<T>(
        &self,
        db: &DB,
        id: Uuid,
        kind: JobKind,
        hash: Option<ScriptHash>,
        job: &mut JobExtended<T>,
    ) {
        let (raw_code, raw_lock, raw_flow) = (
            job.raw_code.take(),
            job.raw_lock.take(),
            job.raw_flow.take(),
        );
        if self.with_flow {
            // Try to fetch the flow from the cache, fallback to the preview flow.
            // NOTE: This could check for the job kinds instead of the `or_else` but it's not
            // necessary as `fetch_flow` return early if the job kind is not a preview one.
            cache::job::fetch_flow(db, kind, hash)
                .or_else(|_| cache::job::fetch_preview_flow(db, &id, raw_flow))
                .await
                .ok()
                .inspect(|data| job.raw_flow = Some(sqlx::types::Json(data.raw_flow.clone())));
        }
        if self.with_code {
            // Try to fetch the code from the cache, fallback to the preview code.
            // NOTE: This could check for the job kinds instead of the `or_else` but it's not
            // necessary as `fetch_script` return early if the job kind is not a preview one.
            cache::job::fetch_script(db, kind, hash)
                .or_else(|_| cache::job::fetch_preview_script(db, &id, raw_lock, raw_code))
                .await
                .ok()
                .inspect(|data| {
                    (job.raw_lock, job.raw_code) = (data.lock.clone(), Some(data.code.clone()))
                });
        }
    }

    async fn fetch_queued(
        self,
        db: &DB,
        job_id: Uuid,
        workspace_id: &str,
    ) -> error::Result<Option<JobExtended<QueuedJob>>> {
        let query = get_job_query!("queue_view",
            with_logs: self.with_logs,
            with_code: self.with_code,
            with_flow: self.with_flow,
        );
        let query = sqlx::query_as::<_, JobExtended<QueuedJob>>(query)
            .bind(job_id)
            .bind(workspace_id)
            .bind(self.with_in_tags);

        let mut job = query.fetch_optional(db).await?;

        self.check_auth(job.as_ref().map(|job| job.created_by.as_str()))?;
        if let Some(job) = job.as_mut() {
            self.resolve_raw_values(db, job.id, job.job_kind, job.script_hash, job)
                .await;
        }
        if self.with_flow {
            job = resolve_maybe_value(db, workspace_id, self.with_code, job, |job| {
                job.raw_flow.as_mut()
            })
            .await?;
        }
        Ok(job)
    }

    async fn fetch_completed(
        self,
        db: &DB,
        job_id: Uuid,
        workspace_id: &str,
    ) -> error::Result<Option<JobExtended<CompletedJob>>> {
        let query = get_job_query!("completed_job_view",
            with_logs: self.with_logs,
            with_code: self.with_code,
            with_flow: self.with_flow,
        );
        let query = sqlx::query_as::<_, JobExtended<CompletedJob>>(query)
            .bind(job_id)
            .bind(workspace_id)
            .bind(self.with_in_tags);

        let mut cjob = query.fetch_optional(db).await?;

        self.check_auth(cjob.as_ref().map(|job| job.created_by.as_str()))?;
        if let Some(job) = cjob.as_mut() {
            self.resolve_raw_values(db, job.id, job.job_kind, job.script_hash, job)
                .await;
        }
        if self.with_flow {
            cjob = resolve_maybe_value(db, workspace_id, self.with_code, cjob, |job| {
                job.raw_flow.as_mut()
            })
            .await?;
        }
        if let Some(mut cjob) = cjob {
            cjob.inner = format_completed_job_result(cjob.inner);
            return Ok(Some(cjob));
        }
        Ok(cjob)
    }

    async fn fetch(self, db: &DB, job_id: Uuid, workspace_id: &str) -> error::Result<Job> {
        let cjob = self
            .fetch_completed(db, job_id, workspace_id)
            .await?
            .map(Job::CompletedJob);

        match cjob {
            Some(cjob) => Ok(cjob),
            None => {
                let job_maybe = self
                    .fetch_queued(db, job_id, workspace_id)
                    .await?
                    .map(Job::QueuedJob);
                not_found_if_none(job_maybe, "Job", job_id.to_string())
            }
        }
    }
}

#[cfg(all(feature = "enterprise", feature = "parquet"))]
async fn get_logs_from_store(
    log_offset: i32,
    logs: &str,
    log_file_index: &Option<Vec<String>>,
) -> Option<error::Result<Body>> {
    if log_offset > 0 {
        if let Some(file_index) = log_file_index.clone() {
            tracing::debug!("Getting logs from store: {file_index:?}");
            if let Some(os) = OBJECT_STORE_CACHE_SETTINGS.read().await.clone() {
                tracing::debug!("object store client present, streaming from there");

                let logs = logs.to_string();
                let stream = async_stream::stream! {
                    yield Ok(bytes::Bytes::from(
                        r#"to remove ansi colors, use: | sed 's/\x1B\[[0-9;]\{1,\}[A-Za-z]//g'
                "#
                        .to_string(),
                    ));
                    for file_p in file_index.clone() {
                        let file_p_2 = file_p.clone();
                        let file = os.get(&object_store::path::Path::from(file_p)).await;
                        if let Ok(file) = file {
                            if let Ok(bytes) = file.bytes().await {
                                yield Ok(bytes::Bytes::from(bytes)) as object_store::Result<bytes::Bytes>;
                            }
                        } else {
                            tracing::debug!("error getting file from store: {file_p_2}: {}", file.err().unwrap());
                        }
                    }

                    yield Ok(bytes::Bytes::from(logs))
                };
                return Some(Ok(Body::from_stream(stream)));
            } else {
                tracing::debug!("object store client not present, cannot stream logs from store");
            }
        }
    }
    return None;
}

async fn get_logs_from_disk(
    log_offset: i32,
    logs: &str,
    log_file_index: &Option<Vec<String>>,
) -> Option<error::Result<Body>> {
    if log_offset > 0 {
        if let Some(file_index) = log_file_index.clone() {
            for file_p in &file_index {
                if !tokio::fs::metadata(format!("{TMP_DIR}/{file_p}"))
                    .await
                    .is_ok()
                {
                    return None;
                }
            }

            let logs = logs.to_string();
            let stream = async_stream::stream! {
                yield Ok(bytes::Bytes::from(
                    r#"to remove ansi colors, use: | sed 's/\x1B\[[0-9;]\{1,\}[A-Za-z]//g'
            "#.to_string(),
                ));
                for file_p in file_index.clone() {
                    let mut file = tokio::fs::File::open(format!("{TMP_DIR}/{file_p}")).await.map_err(to_anyhow)?;
                    let mut buffer = Vec::new();
                    file.read_to_end(&mut buffer).await.map_err(to_anyhow)?;
                    yield Ok(bytes::Bytes::from(buffer)) as anyhow::Result<bytes::Bytes>;
                }

                yield Ok(bytes::Bytes::from(logs))
            };
            return Some(Ok(Body::from_stream(stream)));
        }
    }
    return None;
}

async fn get_job_logs(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
) -> error::Result<Response> {
    // let audit_author: AuditAuthor = match opt_authed {
    //     Some(authed) => (&authed).into(),
    //     None => {
    //         return Err(Error::BadRequest(format!(
    //             "Cancelling script require to be logged in and member of {w_id}"
    //         )))
    //     }
    // };

    let tags = opt_authed
        .as_ref()
        .map(|authed| get_scope_tags(authed).map(|v| v.iter().map(|s| s.to_string()).collect_vec()))
        .flatten();

    let record = sqlx::query!(
        "SELECT created_by, CONCAT(coalesce(completed_job.logs, ''), coalesce(job_logs.logs, '')) as logs, job_logs.log_offset, job_logs.log_file_index
        FROM completed_job 
        LEFT JOIN job_logs ON job_logs.job_id = completed_job.id 
        WHERE completed_job.id = $1 AND completed_job.workspace_id = $2 AND ($3::text[] IS NULL OR completed_job.tag = ANY($3))",
        id,
        w_id,
        tags.as_ref().map(|v| v.as_slice())
    )
    .fetch_optional(&db)
    .await?;

    if let Some(record) = record {
        if opt_authed.is_none() && record.created_by != "anonymous" {
            return Err(Error::BadRequest(
                "As a non logged in user, you can only see jobs ran by anonymous users".to_string(),
            ));
        }
        let logs = record.logs.unwrap_or_default();

        log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

        #[cfg(all(feature = "enterprise", feature = "parquet"))]
        if let Some(r) = get_logs_from_store(record.log_offset, &logs, &record.log_file_index).await
        {
            return r.map(content_plain);
        }
        if let Some(r) = get_logs_from_disk(record.log_offset, &logs, &record.log_file_index).await
        {
            return r.map(content_plain);
        }
        let logs = format!(
            "to remove ansi colors, use: | sed 's/\\x1B\\[[0-9;]\\{{1,\\}}[A-Za-z]//g'\n{}",
            logs
        );
        Ok(content_plain(Body::from(logs)))
    } else {
        let text = sqlx::query!(
            "SELECT created_by, CONCAT(coalesce(queue.logs, ''), coalesce(job_logs.logs, '')) as logs, coalesce(job_logs.log_offset, 0) as log_offset, job_logs.log_file_index
            FROM queue 
            LEFT JOIN job_logs ON job_logs.job_id = queue.id 
            WHERE queue.id = $1 AND queue.workspace_id = $2 AND ($3::text[] IS NULL OR queue.tag = ANY($3))",
            id,
            w_id,
            tags.as_ref().map(|v| v.as_slice())
        )
        .fetch_optional(&db)
        .await?;
        let text = not_found_if_none(text, "Job Logs", id.to_string())?;

        if opt_authed.is_none() && text.created_by != "anonymous" {
            return Err(Error::BadRequest(
                "As a non logged in user, you can only see jobs ran by anonymous users".to_string(),
            ));
        }
        let logs = text.logs.unwrap_or_default();

        log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

        #[cfg(all(feature = "enterprise", feature = "parquet"))]
        if let Some(r) =
            get_logs_from_store(text.log_offset.unwrap_or(0), &logs, &text.log_file_index).await
        {
            return r.map(content_plain);
        }
        if let Some(r) =
            get_logs_from_disk(text.log_offset.unwrap_or(0), &logs, &text.log_file_index).await
        {
            return r.map(content_plain);
        }

        let logs = format!(
            "to remove ansi colors, use: | sed 's/\\x1B\\[[0-9;]\\{{1,\\}}[A-Za-z]//g'\n{}",
            logs
        );
        Ok(content_plain(Body::from(logs)))
    }
}

#[derive(FromRow)]
pub struct RawArgs {
    pub args: Option<sqlx::types::Json<Box<RawValue>>>,
    pub created_by: String,
}

async fn get_args(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
) -> error::JsonResult<Box<serde_json::value::RawValue>> {
    let tags = opt_authed
        .as_ref()
        .map(|authed| get_scope_tags(authed))
        .flatten();
    let record = sqlx::query_as::<_, RawArgs>(
        "SELECT created_by, args
        FROM completed_job 
        WHERE completed_job.id = $1 AND completed_job.workspace_id = $2 AND ($3::text[] IS NULL OR completed_job.tag = ANY($3))",
    )
    .bind(&id)
    .bind(&w_id)
    .bind(tags.as_ref().map(|v| v.as_slice()))
    .fetch_optional(&db)
    .await?;

    if let Some(record) = record {
        if opt_authed.is_none() && record.created_by != "anonymous" {
            return Err(Error::BadRequest(
                "As a non logged in user, you can only see jobs ran by anonymous users".to_string(),
            ));
        }

        log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

        Ok(Json(record.args.map(|x| x.0).unwrap_or_default()))
    } else {
        let record = sqlx::query_as::<_, RawArgs>(
            "SELECT created_by, args
            FROM queue
            WHERE queue.id = $1 AND queue.workspace_id = $2 AND ($3::text[] IS NULL OR queue.tag = ANY($3))",
        )
        .bind(&id)
        .bind(&w_id)
        .bind(tags.as_ref().map(|v| v.as_slice()))
        .fetch_optional(&db)
        .await?;
        let record = not_found_if_none(record, "Job Args", id.to_string())?;
        if opt_authed.is_none() && record.created_by != "anonymous" {
            return Err(Error::BadRequest(
                "As a non logged in user, you can only see jobs ran by anonymous users".to_string(),
            ));
        }

        log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

        Ok(Json(record.args.map(|x| x.0).unwrap_or_default()))
    }
}

#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct ListableCompletedJob {
    pub r#type: String,
    pub workspace_id: String,
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_job: Option<Uuid>,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: i64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_hash: Option<ScriptHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_code: Option<String>,
    pub canceled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canceled_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canceled_reason: Option<String>,
    pub job_kind: JobKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_path: Option<String>,
    pub permissioned_as: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_status: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_flow: Option<serde_json::Value>,
    pub is_flow_step: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<ScriptLang>,
    pub is_skipped: bool,
    pub email: String,
    pub visible_to_owner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_peak: Option<i32>,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone, Default)]
pub struct RunJobQuery {
    pub scheduled_for: Option<chrono::DateTime<chrono::Utc>>,
    pub scheduled_in_secs: Option<i64>,
    pub parent_job: Option<Uuid>,
    pub root_job: Option<Uuid>,
    pub invisible_to_owner: Option<bool>,
    pub queue_limit: Option<i64>,
    pub payload: Option<String>,
    pub job_id: Option<Uuid>,
    pub tag: Option<String>,
    pub timeout: Option<i32>,
    pub cache_ttl: Option<i32>,
    pub skip_preprocessor: Option<bool>,
}

impl RunJobQuery {
    async fn get_scheduled_for<'c>(
        &self,
        db: &DB,
    ) -> error::Result<Option<chrono::DateTime<chrono::Utc>>> {
        if let Some(scheduled_for) = self.scheduled_for {
            Ok(Some(scheduled_for))
        } else if let Some(scheduled_in_secs) = self.scheduled_in_secs {
            let now = now_from_db(db).await?;
            Ok(Some(
                now + chrono::Duration::try_seconds(scheduled_in_secs).unwrap_or_default(),
            ))
        } else {
            Ok(None)
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct ListQueueQuery {
    pub script_path_start: Option<String>,
    pub script_path_exact: Option<String>,
    pub script_hash: Option<String>,
    pub created_by: Option<String>,
    pub started_before: Option<chrono::DateTime<chrono::Utc>>,
    pub started_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_before: Option<chrono::DateTime<chrono::Utc>>,
    pub created_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_or_started_before: Option<chrono::DateTime<chrono::Utc>>,
    pub created_or_started_after: Option<chrono::DateTime<chrono::Utc>>,
    pub running: Option<bool>,
    pub schedule_path: Option<String>,
    pub parent_job: Option<String>,
    pub order_desc: Option<bool>,
    pub job_kinds: Option<String>,
    pub suspended: Option<bool>,
    // filter by matching a subset of the args using base64 encoded json subset
    pub args: Option<String>,
    pub tag: Option<String>,
    pub scheduled_for_before_now: Option<bool>,
    pub all_workspaces: Option<bool>,
    pub is_flow_step: Option<bool>,
    pub has_null_parent: Option<bool>,
    pub is_not_schedule: Option<bool>,
    pub concurrency_key: Option<String>,
}

impl From<ListCompletedQuery> for ListQueueQuery {
    fn from(lcq: ListCompletedQuery) -> Self {
        Self {
            script_path_start: lcq.script_path_start,
            script_path_exact: lcq.script_path_exact,
            script_hash: lcq.script_hash,
            created_by: lcq.created_by,
            started_before: lcq.started_before,
            started_after: lcq.started_after,
            created_before: lcq.created_before,
            created_after: lcq.created_after,
            created_or_started_before: lcq.created_or_started_before,
            created_or_started_after: lcq.created_or_started_after,
            running: lcq.running,
            parent_job: lcq.parent_job,
            order_desc: lcq.order_desc,
            job_kinds: lcq.job_kinds,
            suspended: lcq.suspended,
            args: lcq.args,
            tag: lcq.tag,
            schedule_path: lcq.schedule_path,
            scheduled_for_before_now: lcq.scheduled_for_before_now,
            all_workspaces: lcq.all_workspaces,
            is_flow_step: lcq.is_flow_step,
            has_null_parent: lcq.has_null_parent,
            is_not_schedule: lcq.is_not_schedule,
            concurrency_key: lcq.concurrency_key,
        }
    }
}

pub fn filter_list_queue_query(
    mut sqlb: SqlBuilder,
    lq: &ListQueueQuery,
    w_id: &str,
    join_outstanding_wait_times: bool,
) -> SqlBuilder {
    if join_outstanding_wait_times {
        sqlb.left()
            .join("outstanding_wait_time")
            .on_eq("id", "outstanding_wait_time.job_id");
    }

    if w_id != "admins" || !lq.all_workspaces.is_some_and(|x| x) {
        sqlb.and_where_eq("workspace_id", "?".bind(&w_id));
    }

    if let Some(ps) = &lq.script_path_start {
        sqlb.and_where_like_left("script_path", ps);
    }
    if let Some(p) = &lq.script_path_exact {
        sqlb.and_where_eq("script_path", "?".bind(p));
    }
    if let Some(p) = &lq.schedule_path {
        sqlb.and_where_eq("schedule_path", "?".bind(p));
    }
    if let Some(h) = &lq.script_hash {
        sqlb.and_where_eq("script_hash", "?".bind(h));
    }
    if let Some(cb) = &lq.created_by {
        sqlb.and_where_eq("created_by", "?".bind(cb));
    }
    if let Some(t) = &lq.tag {
        sqlb.and_where_eq("tag", "?".bind(t));
    }
    if let Some(r) = &lq.running {
        sqlb.and_where_eq("running", &r);
    }
    if let Some(pj) = &lq.parent_job {
        sqlb.and_where_eq("parent_job", "?".bind(pj));
    }
    if let Some(dt) = &lq.started_before {
        sqlb.and_where_le("started_at", "?".bind(&dt.to_rfc3339()));
    }
    if let Some(dt) = &lq.started_after {
        sqlb.and_where_ge("started_at", "?".bind(&dt.to_rfc3339()));
    }
    if let Some(fs) = &lq.is_flow_step {
        sqlb.and_where_eq("is_flow_step", fs);
    }
    if let Some(fs) = &lq.has_null_parent {
        if *fs {
            sqlb.and_where_is_null("parent_job");
        }
    }

    if let Some(dt) = &lq.created_before {
        sqlb.and_where_le("created_at", "?".bind(&dt.to_rfc3339()));
    }
    if let Some(dt) = &lq.created_after {
        sqlb.and_where_ge("created_at", "?".bind(&dt.to_rfc3339()));
    }

    if let Some(dt) = &lq.created_or_started_after {
        let ts = dt.timestamp_millis();
        sqlb.and_where(format!("(started_at IS NOT NULL AND started_at >= to_timestamp({}  / 1000.0)) OR (started_at IS NULL AND created_at >= to_timestamp({}  / 1000.0))", ts, ts));
    }

    if let Some(dt) = &lq.created_or_started_before {
        let ts = dt.timestamp_millis();
        sqlb.and_where(format!("(started_at IS NOT NULL AND started_at < to_timestamp({}  / 1000.0)) OR (started_at IS NULL AND created_at < to_timestamp({}  / 1000.0))", ts, ts));
    }

    if let Some(s) = &lq.suspended {
        if *s {
            sqlb.and_where_gt("suspend", 0);
        } else {
            sqlb.and_where_is_null("suspend_until");
        }
    }

    if let Some(jk) = &lq.job_kinds {
        sqlb.and_where_in(
            "job_kind",
            &jk.split(',').into_iter().map(quote).collect::<Vec<_>>(),
        );
    }

    if let Some(args) = &lq.args {
        sqlb.and_where("args @> ?".bind(&args.replace("'", "''")));
    }

    if lq.scheduled_for_before_now.is_some_and(|x| x) {
        sqlb.and_where_le("scheduled_for", "now()");
    }

    if lq.is_not_schedule.unwrap_or(false) {
        sqlb.and_where("schedule_path IS null");
    }

    sqlb
}

pub fn list_queue_jobs_query(
    w_id: &str,
    lq: &ListQueueQuery,
    fields: &[&str],
    pagination: Pagination,
    join_outstanding_wait_times: bool,
    tags: Option<Vec<&str>>,
) -> SqlBuilder {
    let (limit, offset) = paginate_without_limits(pagination);
    let mut sqlb = SqlBuilder::select_from("queue")
        .fields(fields)
        .order_by("created_at", lq.order_desc.unwrap_or(true))
        .limit(limit)
        .offset(offset)
        .clone();

    if let Some(tags) = tags {
        sqlb.and_where_in("tag", &tags.iter().map(|x| quote(x)).collect::<Vec<_>>());
    }

    filter_list_queue_query(sqlb, lq, w_id, join_outstanding_wait_times)
}

#[derive(Serialize, FromRow)]
struct ListableQueuedJob {
    pub id: Uuid,
    pub running: bool,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    pub script_hash: Option<ScriptHash>,
    pub script_path: Option<String>,
    pub args: Option<serde_json::Value>,
    pub job_kind: JobKind,
    pub schedule_path: Option<String>,
    pub is_flow_step: bool,
    pub language: Option<ScriptLang>,
    pub email: String,
    pub suspend: Option<i32>,
    pub tag: String,
    pub priority: Option<i16>,
    pub workspace_id: String,
}

async fn list_queue_jobs(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(pagination): Query<Pagination>,
    Query(lq): Query<ListQueueQuery>,
) -> error::JsonResult<Vec<ListableQueuedJob>> {
    let sql = list_queue_jobs_query(
        &w_id,
        &lq,
        &[
            "id",
            "running",
            "created_by",
            "created_at",
            "started_at",
            "scheduled_for",
            "script_hash",
            "script_path",
            "null as args",
            "job_kind",
            "schedule_path",
            "permissioned_as",
            "is_flow_step",
            "language",
            "same_worker",
            "email",
            "suspend",
            "tag",
            "priority",
            "workspace_id",
        ],
        pagination,
        false,
        get_scope_tags(&authed),
    )
    .sql()?;
    let mut tx = user_db.begin(&authed).await?;
    let jobs = sqlx::query_as::<_, ListableQueuedJob>(&sql)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(jobs))
}

async fn cancel_jobs(
    jobs: Vec<Uuid>,
    db: &DB,
    username: &str,
    w_id: &str,
) -> error::JsonResult<Vec<Uuid>> {
    let mut uuids = vec![];
    let mut tx = db.begin().await?;
    let trivial_jobs =  sqlx::query!("INSERT INTO completed_job AS cj
                   ( workspace_id
                   , id
                   , parent_job
                   , created_by
                   , created_at
                   , started_at
                   , duration_ms
                   , success
                   , script_hash
                   , script_path
                   , args
                   , result
                   , raw_code
                   , raw_lock
                   , canceled
                   , canceled_by
                   , canceled_reason
                   , job_kind
                   , schedule_path
                   , permissioned_as
                   , flow_status
                   , raw_flow
                   , is_flow_step
                   , is_skipped
                   , language
                   , email
                   , visible_to_owner
                   , mem_peak
                   , tag
                   , priority
                )
                SELECT  workspace_id
                   , id
                   , parent_job
                   , created_by
                   , created_at
                   , now()
                   , 0
                   , false
                   , script_hash
                   , script_path
                   , args
                   , $4
                   , raw_code
                   , raw_lock
                   , true
                   , $1
                   , canceled_reason
                   , job_kind
                   , schedule_path
                   , permissioned_as
                   , flow_status
                   , raw_flow
                   , is_flow_step
                   , false
                   , language
                   , email
                   , visible_to_owner
                   , mem_peak
                   , tag
                   , priority FROM queue 
        WHERE id = any($2) AND running = false AND parent_job IS NULL AND workspace_id = $3 AND schedule_path IS NULL FOR UPDATE SKIP LOCKED
        ON CONFLICT (id) DO NOTHING RETURNING id", username, &jobs, w_id, serde_json::json!({"error": { "message": format!("Job canceled: cancel all by {username}"), "name": "Canceled", "reason": "cancel all", "canceler": username}}))
        .fetch_all(&mut *tx)
        .await?.into_iter().map(|x| x.id).collect::<Vec<Uuid>>();

    sqlx::query!(
        "DELETE FROM queue WHERE id = any($1) AND workspace_id = $2",
        &trivial_jobs,
        w_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // sqlx::query!(
    //     "UPDATE queue SET canceled = true, canceled_by = $1, canceled_reason = 'cancelled all by user' WHERE id IN (SELECT id FROM queue where id = any($2) AND workspace_id = $3 AND schedule_path IS NULL FOR UPDATE SKIP LOCKED) RETURNING id",
    //     username,
    //     &jobs,
    //     w_id
    // ).execute(db).await?;
    for job_id in jobs.into_iter() {
        if trivial_jobs.contains(&job_id) {
            continue;
        }
        match tokio::time::timeout(tokio::time::Duration::from_secs(5), async move {
            let tx = db.begin().await?;
            let (tx, _) = windmill_queue::cancel_job(
                username,
                None,
                job_id.clone(),
                w_id,
                tx,
                db,
                false,
                false,
            )
            .await?;
            tx.commit().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        {
            Ok(result) => match result {
                Ok(_) => {
                    uuids.push(job_id);
                }
                Err(e) => {
                    tracing::error!("Failed to cancel job {:?}: {:?}", job_id, e);
                }
            },
            Err(_) => {
                tracing::error!(
                    "Timeout while trying to cancel job {:?} after 5 seconds",
                    job_id
                );
            }
        }
    }

    uuids.extend(trivial_jobs);

    Ok(Json(uuids))
}

async fn cancel_selection(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Json(jobs): Json<Vec<Uuid>>,
) -> error::JsonResult<Vec<Uuid>> {
    let mut tx = user_db.begin(&authed).await?;
    let tags = get_scope_tags(&authed).map(|v| v.iter().map(|s| s.to_string()).collect_vec());
    let jobs_to_cancel = sqlx::query_scalar!(
        "SELECT id FROM queue WHERE id = ANY($1) AND schedule_path IS NULL AND ($2::text[] IS NULL OR tag = ANY($2))",
        &jobs,
        tags.as_ref().map(|v| v.as_slice())
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    cancel_jobs(jobs_to_cancel, &db, authed.username.as_str(), w_id.as_str()).await
}

async fn list_filtered_uuids(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,

    Path(w_id): Path<String>,
    Query(lq): Query<ListQueueQuery>,
) -> error::JsonResult<Vec<Uuid>> {
    require_admin(authed.is_admin, &authed.username)?;

    let mut sqlb = SqlBuilder::select_from("queue").fields(&["id"]).clone();

    sqlb = join_concurrency_key(lq.concurrency_key.as_ref(), sqlb);

    sqlb.and_where_is_null("schedule_path");

    if let Some(tags) = get_scope_tags(&authed) {
        sqlb.and_where_in("tag", &tags.iter().map(|x| quote(x)).collect::<Vec<_>>());
    }

    sqlb = filter_list_queue_query(sqlb, &lq, w_id.as_str(), false);

    let sql = sqlb.query()?;
    let jobs = sqlx::query_scalar(sql.as_str()).fetch_all(&db).await?;

    Ok(Json(jobs))
}

#[derive(Serialize, Debug, FromRow)]
struct QueueStats {
    database_length: i64,
    suspended: Option<i64>,
}

#[derive(Deserialize)]
pub struct CountQueueJobsQuery {
    all_workspaces: Option<bool>,
}

async fn count_queue_jobs(
    Extension(db): Extension<DB>,
    Path(w_id): Path<String>,
    Query(cq): Query<CountQueueJobsQuery>,
) -> error::JsonResult<QueueStats> {
    Ok(Json(
        sqlx::query_as!(
            QueueStats,
            "SELECT coalesce(COUNT(*) FILTER(WHERE suspend = 0 AND running = false), 0) as \"database_length!\", coalesce(COUNT(*) FILTER(WHERE suspend > 0), 0) as \"suspended!\" FROM queue WHERE (workspace_id = $1 OR $2) AND scheduled_for <= now()",
            w_id,
            w_id == "admins" && cq.all_workspaces.unwrap_or(false),
        )
        .fetch_one(&db)
        .await?,
    ))
}

#[derive(Deserialize)]
pub struct CountCompletedJobsQuery {
    completed_after_s_ago: Option<i64>,
    success: Option<bool>,
    tags: Option<String>,
    all_workspaces: Option<bool>,
}

async fn count_completed_jobs_detail(
    Extension(db): Extension<DB>,
    Path(w_id): Path<String>,
    Query(query): Query<CountCompletedJobsQuery>,
) -> error::JsonResult<i64> {
    let mut sqlb = SqlBuilder::select_from("completed_job");
    sqlb.field("COUNT(*) as count");

    if !query.all_workspaces.unwrap_or(false) {
        sqlb.and_where_eq("workspace_id", "?".bind(&w_id));
    }

    if let Some(after_s_ago) = query.completed_after_s_ago {
        let after = Utc::now() - chrono::Duration::seconds(after_s_ago);
        sqlb.and_where_gt(
            "started_at + duration_ms / 1000 * interval '1 second'",
            "?".bind(&after.to_rfc3339()),
        );
    }

    if let Some(success) = query.success {
        sqlb.and_where_eq("success", "?".bind(&success));
    }

    if let Some(tags) = query.tags {
        sqlb.and_where_in(
            "tag",
            &tags
                .split(",")
                .map(|t| format!("'{}'", t))
                .collect::<Vec<_>>(),
        );
    }

    let sql = sqlb.sql()?;
    let stats = sqlx::query_scalar::<_, i64>(&sql).fetch_one(&db).await?;

    Ok(Json(stats))
}

async fn count_completed_jobs(
    Extension(db): Extension<DB>,
    Path(w_id): Path<String>,
) -> error::JsonResult<QueueStats> {
    Ok(Json(
        sqlx::query_as!(
            QueueStats,
            "SELECT coalesce(COUNT(*), 0) as \"database_length!\", null::bigint as suspended FROM completed_job WHERE workspace_id = $1",
            w_id
        )
        .fetch_one(&db)
        .await?,
    ))
}

async fn list_jobs(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(pagination): Query<Pagination>,
    Query(lq): Query<ListCompletedQuery>,
    Extension(_api_list_jobs_query_duration): Extension<Option<Histo>>,
) -> error::JsonResult<Vec<Job>> {
    check_scopes(&authed, || format!("jobs:listjobs"))?;

    let limit = pagination.per_page.unwrap_or(1000);
    let (per_page, offset) = paginate(pagination);
    let lqc = lq.clone();

    if lq.success.is_some() && lq.running.is_some_and(|x| x) {
        return Err(error::Error::BadRequest(
            "cannot specify both success and running".to_string(),
        ));
    }
    let sqlc = if lq.running.is_none() {
        Some(list_completed_jobs_query(
            &w_id,
            per_page + offset,
            0,
            &ListCompletedQuery { order_desc: Some(true), ..lqc },
            UnifiedJob::completed_job_fields(),
            true,
            get_scope_tags(&authed),
        ))
    } else {
        None
    };

    let sql = if lq.success.is_none()
        && lq.label.is_none()
        && lq.created_or_started_before.is_none()
        && lq.started_before.is_none()
    {
        let mut sqlq = list_queue_jobs_query(
            &w_id,
            &ListQueueQuery { order_desc: Some(true), ..lq.into() },
            UnifiedJob::queued_job_fields(),
            Pagination { per_page: Some(limit), page: None },
            true,
            get_scope_tags(&authed),
        );

        if let Some(sqlc) = sqlc {
            format!(
                "{} UNION ALL {} LIMIT {} OFFSET {};",
                &sqlq.subquery()?,
                &sqlc.subquery()?,
                per_page,
                offset
            )
        } else {
            sqlq.limit(per_page).offset(offset).query()?
        }
    } else {
        if sqlc.is_none() {
            return Err(error::Error::BadRequest(
                "cannot specify success, label, created_or_started_before, or started_before with running".to_string(),
            ));
        }
        sqlc.unwrap().limit(per_page).offset(offset).query()?
    };
    let mut tx = user_db.begin(&authed).await?;

    #[cfg(feature = "prometheus")]
    let start = Instant::now();

    #[cfg(feature = "prometheus")]
    if _api_list_jobs_query_duration.is_some() {
        tracing::info!("list_jobs query: {}", sql);
    }

    let jobs: Vec<UnifiedJob> = sqlx::query_as(&sql).fetch_all(&mut *tx).await?;
    tx.commit().await?;

    #[cfg(feature = "prometheus")]
    if let Some(api_list_jobs_query_duration) = _api_list_jobs_query_duration {
        let duration = start.elapsed().as_secs_f64();
        api_list_jobs_query_duration.observe(duration);
        tracing::info!("list_jobs query took {}s: {}", duration, sql);
    }

    Ok(Json(jobs.into_iter().map(From::from).collect()))
}

pub async fn resume_suspended_flow_as_owner(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Path((_w_id, flow_id)): Path<(String, Uuid)>,
    QueryOrBody(value): QueryOrBody<serde_json::Value>,
) -> error::Result<StatusCode> {
    check_scopes(&authed, || format!("jobs:resumeflow"))?;
    let value = value.unwrap_or(serde_json::Value::Null);
    let mut tx = db.begin().await?;

    let (flow, job_id) = get_suspended_flow_info(flow_id, &mut tx).await?;

    require_owner_of_path(
        &authed,
        &flow.script_path.clone().unwrap_or_else(|| String::new()),
    )?;

    insert_resume_job(
        0,
        job_id,
        &flow,
        value,
        Some(authed.username),
        true,
        &mut tx,
    )
    .await?;

    resume_immediately_if_relevant(flow, job_id, &mut tx).await?;

    tx.commit().await?;
    Ok(StatusCode::CREATED)
}

pub async fn resume_suspended_job(
    authed: Option<ApiAuthed>,
    Extension(db): Extension<DB>,
    Path((w_id, job_id, resume_id, secret)): Path<(String, Uuid, u32, String)>,
    Query(approver): Query<QueryApprover>,
    QueryOrBody(value): QueryOrBody<serde_json::Value>,
) -> error::Result<StatusCode> {
    resume_suspended_job_internal(
        value, db, w_id, job_id, resume_id, approver, secret, authed, true,
    )
    .await
}

async fn resume_suspended_job_internal(
    value: Option<serde_json::Value>,
    db: sqlx::Pool<Postgres>,
    w_id: String,
    job_id: Uuid,
    resume_id: u32,
    approver: QueryApprover,
    secret: String,
    authed: Option<ApiAuthed>,
    approved: bool,
) -> Result<StatusCode, Error> {
    let value = value.unwrap_or(serde_json::Value::Null);
    verify_suspended_secret(&w_id, &db, job_id, resume_id, &approver, secret).await?;

    let parent_flow_info = get_suspended_parent_flow_info(job_id, &db).await?;
    let parent_flow = GetQuery::new()
        .without_logs()
        .without_code()
        .without_flow()
        .fetch(&db, parent_flow_info.id, &w_id)
        .await?;
    let flow_status = parent_flow
        .flow_status()
        .ok_or_else(|| anyhow::anyhow!("unable to find the flow status in the flow job"))?;

    let trigger_email = match &parent_flow {
        Job::CompletedJob(job) => &job.email,
        Job::QueuedJob(job) => &job.email,
    };
    conditionally_require_authed_user(authed.clone(), flow_status, trigger_email)?;

    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (SELECT 1 FROM resume_job WHERE id = $1)
        "#,
        Uuid::from_u128(job_id.as_u128() ^ resume_id as u128),
    )
    .fetch_one(&db)
    .await?
    .unwrap_or(false);

    if exists {
        return Err(anyhow::anyhow!("resume request already sent").into());
    }

    let approver = if authed.as_ref().is_none()
        || (approver
            .approver
            .clone()
            .is_some_and(|x| x != "".to_string()))
    {
        approver.approver
    } else {
        authed.as_ref().map(|x| x.username.clone())
    };
    let mut tx: Transaction<'_, Postgres> = db.begin().await?;

    insert_resume_job(
        resume_id,
        job_id,
        &parent_flow_info,
        value,
        approver.clone(),
        approved,
        &mut tx,
    )
    .await?;

    if !approved {
        sqlx::query!(
            "UPDATE queue SET suspend = 0 WHERE id = $1",
            parent_flow_info.id
        )
        .execute(&mut *tx)
        .await?;
    } else {
        resume_immediately_if_relevant(parent_flow_info, job_id, &mut tx).await?;
    }

    let approver = approver.unwrap_or_else(|| "anonymous".to_string());

    let audit_author = match authed {
        Some(authed) => (&authed).into(),
        None => AuditAuthor {
            email: approver.clone(),
            username: approver.clone(),
            username_override: None,
        },
    };
    audit_log(
        &mut *tx,
        &audit_author,
        "jobs.suspend_resume",
        ActionKind::Update,
        &w_id,
        Some(
            &serde_json::json!({
                "approved": approved,
                "job_id": job_id,
                "details": if approved {
                    format!("Approved by {}", &approver)
                } else {
                    format!("Cancelled by {}", &approver)
                }
            })
            .to_string(),
        ),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::CREATED)
}

async fn verify_suspended_secret(
    w_id: &String,
    db: &DB,
    job_id: Uuid,
    resume_id: u32,
    approver: &QueryApprover,
    secret: String,
) -> Result<(), Error> {
    let key = get_workspace_key(w_id, db).await?;
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).map_err(to_anyhow)?;
    mac.update(job_id.as_bytes());
    mac.update(resume_id.to_be_bytes().as_ref());
    if let Some(approver) = approver.approver.clone() {
        mac.update(approver.as_bytes());
    }
    mac.verify_slice(hex::decode(secret)?.as_ref())
        .map_err(|_| anyhow::anyhow!("Invalid signature"))?;
    Ok(())
}

/* If the flow is currently waiting to be resumed (`FlowStatusModule::WaitingForEvents`)
 * the suspend column must be set to the number of resume messages waited on.
 *
 * The flow's queue row is locked in this transaction because to avoid race conditions around
 * the suspend column.
 * That is, a job needs one event but it hasn't arrived, a worker counts zero events before
 * entering WaitingForEvents.  Then this message arrives but the job isn't in WaitingForEvents
 * yet so the suspend counter isn't updated.  Then the job enters WaitingForEvents expecting
 * one event to arrive based on the count that is no longer correct. */
async fn resume_immediately_if_relevant<'c>(
    flow: FlowInfo,
    job_id: Uuid,
    tx: &mut Transaction<'c, Postgres>,
) -> error::Result<()> {
    Ok(
        if let Some(suspend) = (0 < flow.suspend).then(|| flow.suspend - 1) {
            let status =
                serde_json::from_value::<FlowStatus>(flow.flow_status.context("no flow status")?)
                    .context("deserialize flow status")?;
            if matches!(status.current_step(), Some(FlowStatusModule::WaitingForEvents { job, .. }) if job == &job_id)
            {
                sqlx::query!(
                    "UPDATE queue SET suspend = $1 WHERE id = $2",
                    suspend,
                    flow.id,
                )
                .execute(&mut **tx)
                .await?;
            }
        },
    )
}

async fn insert_resume_job<'c>(
    resume_id: u32,
    job_id: Uuid,
    flow: &FlowInfo,
    value: serde_json::Value,
    approver: Option<String>,
    approved: bool,
    tx: &mut Transaction<'c, Postgres>,
) -> error::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO resume_job
                    (id, resume_id, job, flow, value, approver, approved)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        Uuid::from_u128(job_id.as_u128() ^ resume_id as u128),
        resume_id as i32,
        job_id,
        flow.id,
        value,
        approver,
        approved
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct FlowInfo {
    id: Uuid,
    flow_status: Option<serde_json::Value>,
    suspend: i32,
    script_path: Option<String>,
}

async fn get_suspended_parent_flow_info(job_id: Uuid, db: &DB) -> error::Result<FlowInfo> {
    let flow = sqlx::query_as!(
        FlowInfo,
        r#"
        SELECT id, flow_status, suspend, script_path
        FROM queue
        WHERE id = ( SELECT parent_job FROM queue WHERE id = $1 UNION ALL SELECT parent_job FROM completed_job WHERE id = $1)
        FOR UPDATE
        "#,
        job_id,
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("parent flow job not found"))?;
    Ok(flow)
}

async fn get_suspended_flow_info<'c>(
    job_id: Uuid,
    tx: &mut Transaction<'c, Postgres>,
) -> error::Result<(FlowInfo, Uuid)> {
    let flow = sqlx::query_as!(
        FlowInfo,
        r#"
        SELECT id, flow_status, suspend, script_path
        FROM queue
        WHERE id = $1
        "#,
        job_id,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| anyhow::anyhow!("parent flow job not found"))?;
    let job_id = flow
        .flow_status
        .as_ref()
        .and_then(|v| serde_json::from_value::<FlowStatus>(v.clone()).ok())
        .and_then(|s| match s.modules.get(s.step as usize) {
            Some(FlowStatusModule::WaitingForEvents { job, .. }) => Some(job.to_owned()),
            _ => None,
        });

    if let Some(job_id) = job_id {
        Ok((flow, job_id))
    } else {
        Err(anyhow::anyhow!("the flow is not in a suspended state anymore").into())
    }
}

pub async fn cancel_suspended_job(
    authed: Option<ApiAuthed>,
    Extension(db): Extension<DB>,
    Path((w_id, job_id, resume_id, secret)): Path<(String, Uuid, u32, String)>,
    Query(approver): Query<QueryApprover>,
    QueryOrBody(value): QueryOrBody<serde_json::Value>,
) -> error::Result<StatusCode> {
    resume_suspended_job_internal(
        value, db, w_id, job_id, resume_id, approver, secret, authed, false,
    )
    .await
}

#[derive(Serialize)]
pub struct SuspendedJobFlow {
    pub job: Job,
    pub approvers: Vec<Approval>,
}

#[derive(Deserialize, Debug)]
pub struct QueryApprover {
    pub approver: Option<String>,
}

pub async fn get_suspended_job_flow(
    authed: Option<ApiAuthed>,
    Extension(db): Extension<DB>,
    Path((w_id, job, resume_id, secret)): Path<(String, Uuid, u32, String)>,
    Query(approver): Query<QueryApprover>,
) -> error::Result<Response> {
    verify_suspended_secret(&w_id, &db, job, resume_id, &approver, secret).await?;

    let flow_id = sqlx::query_scalar!(
        r#"
        SELECT parent_job
        FROM queue
        WHERE id = $1 AND workspace_id = $2
        UNION ALL
        SELECT parent_job
        FROM completed_job
        WHERE id = $1 AND workspace_id = $2
        "#,
        job,
        w_id
    )
    .fetch_optional(&db)
    .await?
    .flatten()
    .ok_or_else(|| anyhow::anyhow!("parent flow job not found"))?;

    let flow = GetQuery::new()
        .without_logs()
        .without_code()
        .fetch(&db, flow_id, &w_id)
        .await?;

    let flow_status = flow
        .flow_status()
        .ok_or_else(|| anyhow::anyhow!("unable to find the flow status in the flow job"))?;
    let flow_module_status = flow_status
        .modules
        .iter()
        .find(|p| p.job() == Some(job))
        .ok_or_else(|| anyhow::anyhow!("unable to find the module"))?;

    let trigger_email = match &flow {
        Job::CompletedJob(job) => &job.email,
        Job::QueuedJob(job) => &job.email,
    };
    conditionally_require_authed_user(authed.clone(), flow_status.clone(), trigger_email)?;

    let approvers_from_status = match flow_module_status {
        FlowStatusModule::Success { approvers, .. } => approvers.to_owned(),
        _ => vec![],
    };
    let approvers = if approvers_from_status.is_empty() {
        sqlx::query!(
            r#"
            SELECT resume_id, approver
            FROM resume_job
            WHERE job = $1
            "#,
            job,
        )
        .fetch_all(&db)
        .await?
        .into_iter()
        .map(|x| Approval {
            resume_id: x.resume_id as u16,
            approver: x.approver.unwrap_or_else(|| "anonymous".to_string()),
        })
        .collect()
    } else {
        approvers_from_status
    };

    log_job_view(&db, authed.as_ref(), &w_id, &job).await?;

    Ok(Json(SuspendedJobFlow { job: flow, approvers }).into_response())
}

fn conditionally_require_authed_user(
    _authed: Option<ApiAuthed>,
    flow_status: FlowStatus,
    _trigger_email: &str,
) -> error::Result<()> {
    let approval_conditions_opt = flow_status.approval_conditions;

    if approval_conditions_opt.is_none() {
        return Ok(());
    }
    let approval_conditions = approval_conditions_opt.unwrap();

    if approval_conditions.user_auth_required {
        {
            #[cfg(not(feature = "enterprise"))]
            return Err(Error::BadRequest(
                "Approvals for logged in users is an enterprise only feature".to_string(),
            ));

            #[cfg(feature = "enterprise")]
            {
                if _authed.is_none() {
                    return Err(Error::NotAuthorized(
                        "Only logged in users can approve this flow step".to_string(),
                    ));
                }

                let authed = _authed.unwrap();
                if !authed.is_admin {
                    if approval_conditions.self_approval_disabled && authed.email.eq(_trigger_email)
                    {
                        return Err(Error::PermissionDenied(
                            "Self-approval is disabled for this flow step".to_string(),
                        ));
                    }

                    if !approval_conditions.user_groups_required.is_empty() {
                        #[cfg(feature = "enterprise")]
                        {
                            for required_group in approval_conditions.user_groups_required.iter() {
                                if authed.groups.contains(&required_group) {
                                    return Ok(());
                                }
                            }
                            let error_msg = format!("Only users from one of the following groups are allowed to approve this workflow: {}", 
                            approval_conditions.user_groups_required.join(", "));
                            return Err(Error::PermissionDenied(error_msg));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub async fn create_job_signature(
    _authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, job_id, resume_id)): Path<(String, Uuid, u32)>,
    Query(approver): Query<QueryApprover>,
) -> error::Result<String> {
    let key = get_workspace_key(&w_id, &db).await?;
    create_signature(key, job_id, resume_id, approver.approver)
}

pub async fn get_flow_user_state(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, job_id, key)): Path<(String, Uuid, String)>,
) -> error::JsonResult<Option<serde_json::Value>> {
    let mut tx = user_db.begin(&authed).await?;
    let r = sqlx::query_scalar!(
        r#"
        SELECT flow_status->'user_states'->$1
        FROM queue
        WHERE id = $2 AND workspace_id = $3
        "#,
        key,
        job_id,
        w_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    Ok(Json(r))
}

pub async fn set_flow_user_state(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, job_id, key)): Path<(String, Uuid, String)>,
    Json(value): Json<serde_json::Value>,
) -> error::Result<String> {
    let mut tx = user_db.begin(&authed).await?;
    let r = sqlx::query_scalar!(
        r#"
        UPDATE queue SET flow_status = JSONB_SET(flow_status,  ARRAY['user_states'], JSONB_SET(COALESCE(flow_status->'user_states', '{}'::jsonb), ARRAY[$1], $2))
        WHERE id = $3 AND workspace_id = $4 AND job_kind IN ('flow', 'flowpreview', 'flownode') RETURNING 1
        "#,
        key,
        value,
        job_id,
        w_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if r.is_none() {
        return Err(Error::NotFound("Flow job not found".to_string()));
    }
    tx.commit().await?;
    Ok("Flow job state updated".to_string())
}

fn create_signature(
    key: String,
    job_id: Uuid,
    resume_id: u32,
    approver: Option<String>,
) -> Result<String, Error> {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).map_err(to_anyhow)?;
    mac.update(job_id.as_bytes());
    mac.update(resume_id.to_be_bytes().as_ref());
    if let Some(approver) = approver {
        mac.update(approver.as_bytes());
    }
    Ok(hex::encode(mac.finalize().into_bytes()))
}

#[allow(non_snake_case)]
#[derive(Serialize, Debug)]
pub struct ResumeUrls {
    pub approvalPage: String,
    pub cancel: String,
    pub resume: String,
}

fn build_resume_url(
    op: &str,
    w_id: &str,
    job_id: &Uuid,
    resume_id: &u32,
    signature: &str,
    approver: &str,
    base_url: &str,
) -> String {
    format!("{base_url}/api/w/{w_id}/jobs_u/{op}/{job_id}/{resume_id}/{signature}{approver}")
}

pub async fn get_resume_urls(
    _authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, job_id, resume_id)): Path<(String, Uuid, u32)>,
    Query(approver): Query<QueryApprover>,
) -> error::JsonResult<ResumeUrls> {
    get_resume_urls_internal(
        Extension(db),
        Path((w_id, job_id, resume_id)),
        Query(approver),
    )
    .await
}

pub async fn get_resume_urls_internal(
    Extension(db): Extension<DB>,
    Path((w_id, job_id, resume_id)): Path<(String, Uuid, u32)>,
    Query(approver): Query<QueryApprover>,
) -> error::JsonResult<ResumeUrls> {
    let key = get_workspace_key(&w_id, &db).await?;
    let signature = create_signature(key, job_id, resume_id, approver.approver.clone())?;
    let approver = approver
        .approver
        .as_ref()
        .map(|x| format!("?approver={}", encode(x)))
        .unwrap_or_else(String::new);

    let base_url_str = BASE_URL.read().await.clone();
    let base_url = base_url_str.as_str();
    let res = ResumeUrls {
        approvalPage: format!(
            "{base_url}/approve/{w_id}/{job_id}/{resume_id}/{signature}{approver}"
        ),
        cancel: build_resume_url(
            "cancel", &w_id, &job_id, &resume_id, &signature, &approver, &base_url,
        ),
        resume: build_resume_url(
            "resume", &w_id, &job_id, &resume_id, &signature, &approver, &base_url,
        ),
    };

    Ok(Json(res))
}

#[derive(sqlx::FromRow, Debug, Serialize)]
pub struct JobExtended<T> {
    #[sqlx(flatten)]
    #[serde(flatten)]
    inner: T,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_lock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_flow: Option<sqlx::types::Json<Box<RawValue>>>,

    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_wait_time_ms: Option<i64>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_wait_time_ms: Option<i64>,
}

impl<T> JobExtended<T> {
    pub fn new(
        self_wait_time_ms: Option<i64>,
        aggregate_wait_time_ms: Option<i64>,
        inner: T,
    ) -> Self {
        Self {
            inner,
            raw_code: None,
            raw_lock: None,
            raw_flow: None,
            self_wait_time_ms,
            aggregate_wait_time_ms,
        }
    }
}

impl<T> Deref for JobExtended<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for JobExtended<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub enum Job {
    QueuedJob(JobExtended<QueuedJob>),
    CompletedJob(JobExtended<CompletedJob>),
}

impl Job {
    pub fn created_by(&self) -> &str {
        match self {
            Job::QueuedJob(job) => &job.created_by,
            Job::CompletedJob(job) => &job.created_by,
        }
    }

    pub fn append_to_logs(&mut self, logs: &str) {
        match self {
            Job::QueuedJob(job) => {
                if let Some(ref mut l) = job.logs {
                    l.push_str(logs);
                } else {
                    job.logs = Some(logs.to_string());
                }
            }
            Job::CompletedJob(job) => {
                if let Some(ref mut l) = job.logs {
                    l.push_str(logs);
                } else {
                    job.logs = Some(logs.to_string());
                }
            }
        }
    }

    pub fn log_len(&self) -> Option<usize> {
        match self {
            Job::QueuedJob(job) => job.logs.as_ref().map(|l| l.len()),
            Job::CompletedJob(job) => job.logs.as_ref().map(|l| l.len()),
        }
    }

    pub fn logs(&self) -> Option<String> {
        match self {
            Job::QueuedJob(job) => job.logs.clone(),
            Job::CompletedJob(job) => job.logs.clone(),
        }
    }
    pub fn flow_status(&self) -> Option<FlowStatus> {
        match self {
            Job::QueuedJob(job) => job
                .flow_status
                .as_ref()
                .map(|rf| serde_json::from_str(rf.0.get()).ok())
                .flatten(),
            Job::CompletedJob(job) => job
                .flow_status
                .as_ref()
                .map(|rf| serde_json::from_str(rf.0.get()).ok())
                .flatten(),
        }
    }
    pub fn is_flow_step(&self) -> bool {
        match self {
            Job::QueuedJob(job) => job.is_flow_step,
            Job::CompletedJob(job) => job.is_flow_step,
        }
    }

    pub fn is_flow(&self) -> bool {
        self.job_kind().is_flow()
    }

    pub fn job_kind(&self) -> &JobKind {
        match self {
            Job::QueuedJob(job) => &job.job_kind,
            Job::CompletedJob(job) => &job.job_kind,
        }
    }

    pub fn id(&self) -> Uuid {
        match self {
            Job::QueuedJob(job) => job.id,
            Job::CompletedJob(job) => job.id,
        }
    }

    pub fn workspace_id(&self) -> &String {
        match self {
            Job::QueuedJob(job) => &job.workspace_id,
            Job::CompletedJob(job) => &job.workspace_id,
        }
    }

    pub fn script_path(&self) -> &str {
        match self {
            Job::QueuedJob(job) => job.script_path.as_ref(),
            Job::CompletedJob(job) => job.script_path.as_ref(),
        }
        .map(String::as_str)
        .unwrap_or("tmp/main")
    }

    pub fn args(&self) -> Option<&sqlx::types::Json<HashMap<String, Box<RawValue>>>> {
        match self {
            Job::QueuedJob(job) => job.args.as_ref(),
            Job::CompletedJob(job) => job.args.as_ref(),
        }
    }

    pub fn full_path_with_workspace(&self) -> String {
        format!(
            "{}/{}/{}",
            self.workspace_id(),
            if self.is_flow() { "flow" } else { "script" },
            self.script_path(),
        )
    }

    pub async fn concurrency_key(
        &self,
        db: &Pool<Postgres>,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar!(
            "SELECT key FROM concurrency_key WHERE job_id = $1",
            self.id()
        )
        .fetch_optional(db)
        .await
    }

    pub async fn fetch_outstanding_wait_time(
        &mut self,
        db: &Pool<Postgres>,
    ) -> Result<(), sqlx::Error> {
        let r = sqlx::query!(
            "SELECT self_wait_time_ms, aggregate_wait_time_ms FROM outstanding_wait_time WHERE job_id = $1",
            self.id()
        )
        .fetch_optional(db)
        .await?;

        let (self_wait_time, aggregate_wait_time) = r
            .map(|x| (x.self_wait_time_ms, x.aggregate_wait_time_ms))
            .unwrap_or((None, None));

        match self {
            Job::QueuedJob(job) => {
                job.self_wait_time_ms = self_wait_time;
                job.aggregate_wait_time_ms = aggregate_wait_time;
            }
            Job::CompletedJob(job) => {
                job.self_wait_time_ms = self_wait_time;
                job.aggregate_wait_time_ms = aggregate_wait_time;
            }
        }
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
pub struct UnifiedJob {
    pub workspace_id: String,
    pub typ: String,
    pub id: Uuid,
    pub parent_job: Option<Uuid>,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub scheduled_for: Option<chrono::DateTime<chrono::Utc>>,
    pub running: Option<bool>,
    pub script_hash: Option<ScriptHash>,
    pub script_path: Option<String>,
    pub duration_ms: Option<i64>,
    pub success: Option<bool>,
    pub deleted: bool,
    pub canceled: bool,
    pub canceled_by: Option<String>,
    pub job_kind: JobKind,
    pub schedule_path: Option<String>,
    pub permissioned_as: String,
    pub is_flow_step: bool,
    pub language: Option<ScriptLang>,
    pub is_skipped: bool,
    pub email: String,
    pub visible_to_owner: bool,
    pub suspend: Option<i32>,
    pub mem_peak: Option<i32>,
    pub tag: String,
    pub concurrent_limit: Option<i32>,
    pub concurrency_time_window_s: Option<i32>,
    pub priority: Option<i16>,
    pub labels: Option<serde_json::Value>,
    pub self_wait_time_ms: Option<i64>,
    pub aggregate_wait_time_ms: Option<i64>,
}

const CJ_FIELDS: &[&str] = &[
    "'CompletedJob' as typ",
    "id",
    "workspace_id",
    "parent_job",
    "created_by",
    "created_at",
    "started_at",
    "null as scheduled_for",
    "null as running",
    "script_hash",
    "script_path",
    "null as args",
    "duration_ms",
    "success",
    "deleted",
    "canceled",
    "canceled_by",
    "job_kind",
    "schedule_path",
    "permissioned_as",
    "is_flow_step",
    "language",
    "is_skipped",
    "email",
    "visible_to_owner",
    "null as suspend",
    "mem_peak",
    "tag",
    "null as concurrent_limit",
    "null as concurrency_time_window_s",
    "priority",
    "result->'wm_labels' as labels",
    "self_wait_time_ms",
    "aggregate_wait_time_ms",
];
const QJ_FIELDS: &[&str] = &[
    "'QueuedJob' as typ",
    "id",
    "workspace_id",
    "parent_job",
    "created_by",
    "created_at",
    "started_at",
    "scheduled_for",
    "running",
    "script_hash",
    "script_path",
    "null as args",
    "null as duration_ms",
    "null as success",
    "false as deleted",
    "canceled",
    "canceled_by",
    "job_kind",
    "schedule_path",
    "permissioned_as",
    "is_flow_step",
    "language",
    "false as is_skipped",
    "email",
    "visible_to_owner",
    "suspend",
    "mem_peak",
    "tag",
    "concurrent_limit",
    "concurrency_time_window_s",
    "priority",
    "null as labels",
    "self_wait_time_ms",
    "aggregate_wait_time_ms",
];

impl UnifiedJob {
    pub fn completed_job_fields() -> &'static [&'static str] {
        CJ_FIELDS
    }
    pub fn queued_job_fields() -> &'static [&'static str] {
        QJ_FIELDS
    }
}

impl<'a> From<UnifiedJob> for Job {
    fn from(uj: UnifiedJob) -> Self {
        match uj.typ.as_ref() {
            "CompletedJob" => Job::CompletedJob(JobExtended::new(
                uj.self_wait_time_ms,
                uj.aggregate_wait_time_ms,
                CompletedJob {
                    workspace_id: uj.workspace_id,
                    id: uj.id,
                    parent_job: uj.parent_job,
                    created_by: uj.created_by,
                    created_at: uj.created_at,
                    started_at: uj.started_at.unwrap_or(uj.created_at),
                    duration_ms: uj.duration_ms.unwrap(),
                    success: uj.success.unwrap(),
                    script_hash: uj.script_hash,
                    script_path: uj.script_path,
                    args: None,
                    result: None,
                    logs: None,
                    flow_status: None,
                    deleted: uj.deleted,
                    canceled: uj.canceled,
                    canceled_by: uj.canceled_by,
                    canceled_reason: None,
                    job_kind: uj.job_kind,
                    schedule_path: uj.schedule_path,
                    permissioned_as: uj.permissioned_as,
                    is_flow_step: uj.is_flow_step,
                    language: uj.language,
                    is_skipped: uj.is_skipped,
                    email: uj.email,
                    visible_to_owner: uj.visible_to_owner,
                    mem_peak: uj.mem_peak,
                    tag: uj.tag,
                    priority: uj.priority,
                    labels: uj.labels,
                },
            )),
            "QueuedJob" => Job::QueuedJob(JobExtended::new(
                uj.self_wait_time_ms,
                uj.aggregate_wait_time_ms,
                QueuedJob {
                    workspace_id: uj.workspace_id,
                    id: uj.id,
                    parent_job: uj.parent_job,
                    created_by: uj.created_by,
                    created_at: uj.created_at,
                    started_at: uj.started_at,
                    script_hash: uj.script_hash,
                    script_path: uj.script_path,
                    args: None,
                    running: uj.running.unwrap(),
                    scheduled_for: uj.scheduled_for.unwrap(),
                    logs: None,
                    flow_status: None,
                    canceled: uj.canceled,
                    canceled_by: uj.canceled_by,
                    canceled_reason: None,
                    last_ping: None,
                    job_kind: uj.job_kind,
                    schedule_path: uj.schedule_path,
                    permissioned_as: uj.permissioned_as,
                    is_flow_step: uj.is_flow_step,
                    language: uj.language,
                    same_worker: false,
                    pre_run_error: None,
                    email: uj.email,
                    visible_to_owner: uj.visible_to_owner,
                    suspend: uj.suspend,
                    mem_peak: uj.mem_peak,
                    root_job: None,
                    leaf_jobs: None,
                    tag: uj.tag,
                    concurrent_limit: uj.concurrent_limit,
                    concurrency_time_window_s: uj.concurrency_time_window_s,
                    timeout: None,
                    flow_step_id: None,
                    cache_ttl: None,
                    priority: uj.priority,
                },
            )),
            t => panic!("job type {} not valid", t),
        }
    }
}
#[derive(Deserialize)]
struct CancelJob {
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum PreviewKind {
    Code,
    Identity,
    Http,
    Noop,
    Bundle,
    Tarbundle,
}

#[derive(Deserialize)]
struct Preview {
    content: Option<String>,
    kind: Option<PreviewKind>,
    path: Option<String>,
    args: Option<HashMap<String, Box<JsonRawValue>>>,
    language: Option<ScriptLang>,
    tag: Option<String>,
    dedicated_worker: Option<bool>,
    lock: Option<String>,
}

#[derive(Deserialize)]
pub struct WorkflowTask {
    pub args: Option<HashMap<String, Box<JsonRawValue>>>,
}

#[derive(Deserialize)]
struct PreviewFlow {
    value: FlowValue,
    path: Option<String>,
    args: Option<HashMap<String, Box<JsonRawValue>>>,
    tag: Option<String>,
    restarted_from: Option<RestartedFrom>,
}

pub struct QueryOrBody<D>(pub Option<D>);

#[axum::async_trait]
impl<S, D> FromRequest<S, axum::body::Body> for QueryOrBody<D>
where
    D: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(
        req: Request<axum::body::Body>,
        state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        return if req.method() == axum::http::Method::GET {
            let Query(InPayload { payload }) = Query::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            payload
                .map(|p| {
                    decode_payload(p)
                        .map(QueryOrBody)
                        .map_err(|err| (StatusCode::BAD_REQUEST, format!("{err:#?}")))
                        .map_err(IntoResponse::into_response)
                })
                .unwrap_or(Ok(QueryOrBody(None)))
        } else {
            Json::from_request(req, state)
                .await
                .map(|Json(v)| QueryOrBody(Some(v)))
                .map_err(IntoResponse::into_response)
        };

        #[derive(Deserialize)]
        struct InPayload {
            payload: Option<String>,
        }
    }
}

fn decode_payload<D: DeserializeOwned>(t: String) -> anyhow::Result<D> {
    let vec = base64::engine::general_purpose::URL_SAFE
        .decode(t)
        .context("invalid base64")?;
    serde_json::from_slice(vec.as_slice()).context("invalid json")
}

pub fn add_raw_string(
    raw_string: Option<String>,
    mut args: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    if let Some(raw_string) = raw_string {
        args.insert(
            "raw_string".to_string(),
            serde_json::Value::String(raw_string),
        );
    }
    return args;
}

async fn check_tag_available_for_workspace(
    w_id: &str,
    tag: &Option<String>,
    authed: &ApiAuthed,
) -> error::Result<()> {
    if let Some(tag) = tag {
        if tag == "" {
            return Ok(());
        }

        let tags = get_scope_tags(authed);

        if let Some(tags) = tags {
            if !tags.contains(&tag.as_str()) {
                return Err(Error::BadRequest(format!(
                    "Tag {tag} is not available in your scope"
                )));
            }
        }

        let custom_tags_per_w = CUSTOM_TAGS_PER_WORKSPACE.read().await;
        if custom_tags_per_w.0.contains(&tag.to_string()) {
            Ok(())
        } else if custom_tags_per_w.1.contains_key(tag)
            && custom_tags_per_w
                .1
                .get(tag)
                .unwrap()
                .contains(&w_id.to_string())
        {
            Ok(())
        } else {
            return Err(error::Error::BadRequest(format!(
                "Tag {tag} cannot be used on workspace {w_id}: (CUSTOM_TAGS: {:?})",
                custom_tags_per_w
            )));
        }
    } else {
        Ok(())
    }
}

#[cfg(feature = "enterprise")]
pub async fn check_license_key_valid() -> error::Result<()> {
    use windmill_common::ee::LICENSE_KEY_VALID;

    let valid = *LICENSE_KEY_VALID.read().await;
    if !valid {
        return Err(Error::BadRequest(
            "License key is not valid. Go to your superadmin settings to update your license key."
                .to_string(),
        ));
    }
    Ok(())
}

pub async fn run_flow_by_path(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, flow_path)): Path<(String, StripPath)>,
    Query(run_query): Query<RunJobQuery>,
    args: WebhookArgs,
) -> error::Result<(StatusCode, String)> {
    let args = args.to_push_args_owned(&authed, &db, &w_id).await?;

    run_flow_by_path_inner(authed, db, user_db, w_id, flow_path, run_query, args, None).await
}

pub async fn run_flow_by_path_inner(
    authed: ApiAuthed,
    db: DB,
    user_db: UserDB,
    w_id: String,
    flow_path: StripPath,
    run_query: RunJobQuery,
    args: PushArgsOwned,
    label_prefix: Option<String>,
) -> error::Result<(StatusCode, String)> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;
    let flow_path = flow_path.to_path();
    check_scopes(&authed, || format!("run:flow/{flow_path}"))?;

    let mut tx = user_db.clone().begin(&authed).await?;
    let (tag, dedicated_worker, has_preprocessor, on_behalf_of_email, edited_by) = sqlx::query!(
        "SELECT tag, dedicated_worker, flow_version.value->>'preprocessor_module' IS NOT NULL as has_preprocessor, on_behalf_of_email, edited_by
        FROM flow 
        LEFT JOIN flow_version
            ON flow_version.id = flow.versions[array_upper(flow.versions, 1)]
        WHERE flow.path = $1 and flow.workspace_id = $2",
        flow_path,
        w_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .map(|x| (x.tag, x.dedicated_worker, x.has_preprocessor, x.on_behalf_of_email, x.edited_by))
    .ok_or_else(|| {
        Error::NotFound(format!(
            "flow not found at path {flow_path} in workspace {w_id}"
        ))
    })?;
    drop(tx);

    let tag = run_query.tag.clone().or(tag);

    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;
    let scheduled_for = run_query.get_scheduled_for(&db).await?;

    let (email, permissioned_as, push_authed, tx) =
        if let Some(on_behalf_of_email) = on_behalf_of_email.as_ref() {
            (
                on_behalf_of_email,
                username_to_permissioned_as(&edited_by),
                None,
                PushIsolationLevel::IsolatedRoot(db.clone()),
            )
        } else {
            (
                &authed.email,
                username_to_permissioned_as(&authed.username),
                Some(authed.clone().into()),
                PushIsolationLevel::Isolated(user_db, authed.clone().into()),
            )
        };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        JobPayload::Flow {
            path: flow_path.to_string(),
            dedicated_worker,
            apply_preprocessor: !run_query.skip_preprocessor.unwrap_or(false)
                && has_preprocessor.unwrap_or(false),
        },
        PushArgs { args: &args.args, extra: args.extra },
        &label_prefix
            .map(|x| x + authed.display_username())
            .unwrap_or_else(|| authed.display_username().to_string()),
        email,
        permissioned_as,
        scheduled_for,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        None,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, uuid.to_string()))
}

#[cfg(not(feature = "enterprise"))]
pub async fn restart_flow(
    _authed: ApiAuthed,
    Extension(_db): Extension<DB>,
    Extension(_user_db): Extension<UserDB>,
    Path((_w_id, _job_id, _step_id, _branch_or_iteration_n)): Path<(
        String,
        Uuid,
        String,
        Option<usize>,
    )>,
    Query(_run_query): Query<RunJobQuery>,
) -> error::Result<(StatusCode, String)> {
    return Err(Error::BadRequest(
        "Restarting a flow is a feature only available in enterprise version".to_string(),
    ));
}

#[cfg(feature = "enterprise")]
pub async fn restart_flow(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, job_id, step_id, branch_or_iteration_n)): Path<(
        String,
        Uuid,
        String,
        Option<usize>,
    )>,
    Query(run_query): Query<RunJobQuery>,
) -> error::Result<(StatusCode, String)> {
    check_license_key_valid().await?;

    let mut tx = user_db.clone().begin(&authed).await?;
    let completed_job = sqlx::query_as::<_, CompletedJob>(
        "SELECT *, result->'wm_labels' as labels from completed_job WHERE id = $1 and workspace_id = $2",
    )
    .bind(job_id)
    .bind(&w_id)
    .fetch_optional(&mut *tx)
    .await?
    .with_context(|| "Unable to find completed job with the given job UUID")?;
    drop(tx);

    let flow_path = completed_job
        .script_path
        .with_context(|| "No flow path set for completed flow job")?;
    check_scopes(&authed, || format!("run:flow/{flow_path}"))?;

    let ehm = HashMap::new();
    let push_args = completed_job
        .args
        .as_ref()
        .map(|json| PushArgs { args: &json.0, extra: None })
        .unwrap_or_else(|| PushArgs::from(&ehm));

    let scheduled_for = run_query.get_scheduled_for(&db).await?;

    let tx = PushIsolationLevel::Isolated(user_db, authed.clone().into());

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        JobPayload::RestartedFlow {
            completed_job_id: job_id,
            step_id: step_id,
            branch_or_iteration_n: branch_or_iteration_n,
        },
        push_args,
        &authed.username,
        &authed.email,
        username_to_permissioned_as(&authed.username),
        scheduled_for,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        Some(completed_job.tag),
        None,
        None,
        completed_job.priority,
        Some(&authed.clone().into()),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, uuid.to_string()))
}

pub async fn run_script_by_path(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, script_path)): Path<(String, StripPath)>,
    Query(run_query): Query<RunJobQuery>,
    args: WebhookArgs,
) -> error::Result<(StatusCode, String)> {
    let args = args.to_push_args_owned(&authed, &db, &w_id).await?;
    run_script_by_path_inner(
        authed,
        db,
        user_db,
        w_id,
        script_path,
        run_query,
        args,
        None,
    )
    .await
}

pub async fn run_script_by_path_inner(
    authed: ApiAuthed,
    db: DB,
    user_db: UserDB,
    w_id: String,
    script_path: StripPath,
    run_query: RunJobQuery,
    args: PushArgsOwned,
    label_prefix: Option<String>,
) -> error::Result<(StatusCode, String)> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    let script_path = script_path.to_path();

    check_scopes(&authed, || format!("run:script/{script_path}"))?;

    let mut tx = user_db.clone().begin(&authed).await?;
    let (job_payload, tag, _delete_after_use, timeout, on_behalf_of) =
        script_path_to_payload(script_path, &mut *tx, &w_id, run_query.skip_preprocessor).await?;
    drop(tx);
    let scheduled_for = run_query.get_scheduled_for(&db).await?;

    let tag = run_query.tag.clone().or(tag);
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;

    let (email, permissioned_as, push_authed, tx) =
        if let Some(on_behalf_of) = on_behalf_of.as_ref() {
            (
                on_behalf_of.email.as_str(),
                on_behalf_of.permissioned_as.clone(),
                None,
                PushIsolationLevel::IsolatedRoot(db.clone()),
            )
        } else {
            (
                authed.email.as_str(),
                username_to_permissioned_as(&authed.username),
                Some(authed.clone().into()),
                PushIsolationLevel::Isolated(user_db, authed.clone().into()),
            )
        };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        job_payload,
        PushArgs { args: &args.args, extra: args.extra },
        &label_prefix
            .map(|x| x + authed.display_username())
            .unwrap_or_else(|| authed.display_username().to_string()),
        email,
        permissioned_as,
        scheduled_for,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        timeout,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, uuid.to_string()))
}

#[derive(Deserialize)]
pub struct WorkflowAsCodeQuery {
    pub skip_update: Option<bool>,
}

pub async fn run_workflow_as_code(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, job_id, entrypoint)): Path<(String, Uuid, String)>,
    Query(run_query): Query<RunJobQuery>,
    Query(wkflow_query): Query<WorkflowAsCodeQuery>,
    Json(task): Json<WorkflowTask>,
) -> error::Result<(StatusCode, String)> {
    let mut i = 1;

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;
    check_tag_available_for_workspace(&w_id, &run_query.tag, &authed).await?;

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    let job = GetQuery::new()
        .without_logs()
        .fetch_queued(&db, job_id, &w_id)
        .await?;

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    let job = not_found_if_none(job, "Queued Job", &job_id.to_string())?;
    let JobExtended { inner: job, raw_code, raw_lock, .. } = job;
    let (job_payload, tag, _delete_after_use, timeout, on_behalf_of) = match job.job_kind {
        JobKind::Preview => (
            JobPayload::Code(RawCode {
                hash: None,
                content: raw_code.unwrap_or_default(),
                path: job.script_path,
                language: job.language.unwrap_or_else(|| ScriptLang::Deno),
                lock: raw_lock,
                custom_concurrency_key: windmill_queue::custom_concurrency_key(&db, job.id)
                    .await
                    .map_err(to_anyhow)?,
                concurrent_limit: job.concurrent_limit,
                concurrency_time_window_s: job.concurrency_time_window_s,
                cache_ttl: job.cache_ttl,
                dedicated_worker: None,
            }),
            Some(job.tag.clone()),
            None,
            run_query.timeout,
            None,
        ),
        JobKind::Script => {
            let mut tx = user_db.clone().begin(&authed).await?;
            script_path_to_payload(
                job.script_path(),
                &mut *tx,
                &w_id,
                run_query.skip_preprocessor,
            )
            .await?
        }
        _ => return Err(anyhow::anyhow!("Not supported").into()),
    };

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    let mut extra = HashMap::new();
    extra.insert(ENTRYPOINT_OVERRIDE.to_string(), to_raw_value(&entrypoint));

    let args = PushArgs { args: &task.args.unwrap_or_else(HashMap::new), extra: Some(extra) };
    let scheduled_for = run_query.get_scheduled_for(&db).await?;

    let tag = run_query.tag.clone().or(tag).or(Some(job.tag));

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    let (email, permissioned_as, push_authed, tx) =
        if let Some(on_behalf_of) = on_behalf_of.as_ref() {
            (
                on_behalf_of.email.as_str(),
                on_behalf_of.permissioned_as.clone(),
                None,
                PushIsolationLevel::IsolatedRoot(db.clone()),
            )
        } else {
            (
                authed.email.as_str(),
                username_to_permissioned_as(&authed.username),
                Some(authed.clone().into()),
                PushIsolationLevel::Isolated(user_db, authed.clone().into()),
            )
        };

    let (uuid, mut tx) = push(
        &db,
        tx,
        &w_id,
        job_payload,
        PushArgs { args: &args.args, extra: args.extra },
        authed.display_username(),
        email,
        permissioned_as,
        scheduled_for,
        None,
        Some(job_id),
        job.root_job.or(Some(job_id)),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        timeout,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    if !wkflow_query.skip_update.unwrap_or(false) {
        sqlx::query!(
            "UPDATE queue SET flow_status = jsonb_set(COALESCE(flow_status, '{}'::jsonb), array[$1], jsonb_set(jsonb_set('{}'::jsonb, '{scheduled_for}', to_jsonb(now()::text)), '{name}', to_jsonb($4::text))) WHERE id = $2 AND workspace_id = $3",
            uuid.to_string(),
            job_id,
            w_id,
            entrypoint
        ).execute(&mut *tx).await?;
    } else {
        tracing::info!("Skipping update of flow status for job {job_id} in workspace {w_id}");
    }

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
        i += 1;
    }

    tx.commit().await?;

    if *CLOUD_HOSTED {
        tracing::info!("workflow_as_code_tracing id {i} ");
    }

    Ok((StatusCode::CREATED, uuid.to_string()))
}

struct Guard {
    done: bool,
    id: Uuid,
    w_id: String,
    db: DB,
    username: String,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !&self.done {
            let id = self.id;
            let w_id = self.w_id.clone();
            let db = self.db.clone();
            let username = self.username.clone();

            tracing::info!("http connection broke, marking job {id} as canceled");
            tokio::spawn(async move {
                let cancel_f = async {
                    let tx = db.begin().await?;
                    let (tx, _) = cancel_job(
                        &username,
                        Some("http connection broke".to_string()),
                        id,
                        &w_id,
                        tx,
                        &db,
                        false,
                        false,
                    )
                    .await?;
                    tx.commit().await?;
                    Ok::<_, anyhow::Error>(())
                };

                if let Err(e) = cancel_f.await {
                    tracing::error!(
                        "Error marking job as canceled after http connection broke: {e}"
                    );
                }
            });
        }
    }
}

use std::sync::Arc;
use tokio::sync::RwLock;

lazy_static::lazy_static! {
    pub static ref TIMEOUT_WAIT_RESULT: Arc<RwLock<Option<u64>>> = Arc::new(RwLock::new(
        std::env::var("TIMEOUT_WAIT_RESULT")
            .ok()
            .and_then(|x| x.parse::<u64>().ok())
    ));
}

#[derive(Deserialize)]
pub struct WindmillCompositeResult {
    windmill_status_code: Option<u16>,
    windmill_content_type: Option<String>,
    windmill_headers: Option<HashMap<String, String>>,
    result: Option<Box<RawValue>>,
}

pub async fn run_wait_result_internal(
    db: &DB,
    uuid: Uuid,
    w_id: String,
    node_id_for_empty_return: Option<String>,
    username: &str,
) -> error::Result<(Box<RawValue>, bool)> {
    let mut result = None;
    let mut success = false;
    let timeout = TIMEOUT_WAIT_RESULT.read().await.clone().unwrap_or(600);
    let timeout_ms = if timeout <= 0 {
        2000
    } else {
        (timeout * 1000) as u64
    };

    let mut g = Guard {
        done: false,
        id: uuid,
        w_id: w_id.clone(),
        db: db.clone(),
        username: username.to_string(),
    };

    let fast_poll_duration = *WAIT_RESULT_FAST_POLL_DURATION_SECS as u64 * 1000;
    let mut accumulated_delay = 0 as u64;

    loop {
        if let Some(node_id_for_empty_return) = node_id_for_empty_return.as_ref() {
            let result_and_success = get_result_and_success_by_id_from_flow(
                &db,
                &w_id,
                &uuid,
                node_id_for_empty_return,
                None,
            )
            .await
            .ok();
            if let Some((r, s)) = result_and_success {
                result = Some(r);
                success = s;
            }
        }

        if result.is_none() {
            let row = sqlx::query_as::<_, RawResultWithSuccess>(
                "SELECT '' as created_by, result, language, flow_status, success FROM completed_job WHERE id = $1 AND workspace_id = $2",
            )
            .bind(uuid)
            .bind(&w_id)
            .fetch_optional(db)
            .await?;
            if let Some(mut raw_result) = row {
                format_result(
                    raw_result.language.as_ref(),
                    raw_result.flow_status.as_ref(),
                    raw_result.result.as_mut(),
                );
                result = raw_result.result.map(|x| x.0);
                success = raw_result.success;
            }
        }

        if result.is_some() {
            break;
        }

        let delay = if accumulated_delay <= fast_poll_duration {
            *WAIT_RESULT_FAST_POLL_INTERVAL_MS
        } else {
            *WAIT_RESULT_SLOW_POLL_INTERVAL_MS
        };
        accumulated_delay += delay;
        if accumulated_delay > timeout_ms {
            break;
        };
        tokio::time::sleep(core::time::Duration::from_millis(delay)).await;
    }

    if let Some(result) = result {
        g.done = true;
        Ok((result, success))
    } else {
        Err(Error::ExecutionErr(format!("timeout after {}s", timeout)))
    }
}

pub async fn run_wait_result(
    db: &DB,
    uuid: Uuid,
    w_id: String,
    node_id_for_empty_return: Option<String>,
    username: &str,
) -> error::Result<Response> {
    let (result, success) =
        run_wait_result_internal(db, uuid, w_id, node_id_for_empty_return, username).await?;

    let composite_result = serde_json::from_str::<WindmillCompositeResult>(result.get());
    match composite_result {
        Ok(WindmillCompositeResult {
            windmill_status_code,
            windmill_content_type,
            windmill_headers,
            result: result_value,
        }) => {
            if windmill_content_type.is_none()
                && windmill_status_code.is_none()
                && windmill_headers.is_none()
            {
                return Ok((
                    if success {
                        StatusCode::OK
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    },
                    Json(result),
                )
                    .into_response());
            }

            let status_code_or_default = windmill_status_code
                .map(|val| match StatusCode::from_u16(val) {
                    Ok(sc) => Ok(sc),
                    Err(_) => Err(Error::ExecutionErr("Invalid status code".to_string())),
                })
                .unwrap_or_else(|| {
                    if !success {
                        Ok(StatusCode::INTERNAL_SERVER_ERROR)
                    } else if result_value.is_some() {
                        Ok(StatusCode::OK)
                    } else {
                        Ok(StatusCode::NO_CONTENT)
                    }
                })?;

            let mut headers = HeaderMap::new();

            if let Some(windmill_headers) = windmill_headers {
                for (k, v) in windmill_headers {
                    let k = HeaderName::from_str(k.as_str()).map_err(|err| {
                        Error::InternalErr(format!("Invalid header name {k}: {err}"))
                    })?;
                    let v = HeaderValue::from_str(v.as_str()).map_err(|err| {
                        Error::InternalErr(format!("Invalid header value {v}: {err}"))
                    })?;
                    headers.insert(k, v);
                }
            }

            if let Some(content_type) = windmill_content_type {
                let serialized_json_result = result_value
                    .map(|val| val.get().to_owned())
                    .unwrap_or_else(String::new);
                // if the `result` was just a single string, the below removes the surrounding quotes by parsing it as a string.
                // it falls back to the original serialized JSON if it doesn't work.
                let serialized_result =
                    serde_json::from_str::<String>(serialized_json_result.as_str())
                        .ok()
                        .unwrap_or(serialized_json_result);
                headers.insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_str(content_type.as_str()).map_err(|err| {
                        Error::InternalErr(format!("Invalid content type {content_type}: {err}"))
                    })?,
                );
                return Ok((status_code_or_default, headers, serialized_result).into_response());
            }
            if let Some(result_value) = result_value {
                return Ok((status_code_or_default, headers, Json(result_value)).into_response());
            } else {
                Ok((status_code_or_default, headers).into_response())
            }
        }
        _ => Ok((
            if success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            },
            Json(result),
        )
            .into_response()),
    }
}

async fn delete_job_metadata_after_use(db: &DB, job_uuid: Uuid) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE completed_job
        SET logs = '##DELETED##', args = '{}'::jsonb, result = '{}'::jsonb
        WHERE id = $1",
        job_uuid,
    )
    .execute(db)
    .await?;
    sqlx::query!(
        "UPDATE job_logs
        SET logs = '##DELETED##'
        WHERE job_id = $1",
        job_uuid,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn check_queue_too_long(db: &DB, queue_limit: Option<i64>) -> error::Result<()> {
    if let Some(limit) = queue_limit {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM queue WHERE  canceled = false AND (scheduled_for <= now()
        OR (suspend_until IS NOT NULL
            AND (   suspend <= 0
                 OR suspend_until <= now())))",
        )
        .fetch_one(db)
        .await?
        .unwrap_or(0);

        if count > queue_limit.unwrap() {
            return Err(Error::InternalErr(format!(
                "Number of queued job is too high: {count} > {limit}"
            )));
        }
    }
    Ok(())
}

lazy_static::lazy_static! {
    pub static ref QUEUE_LIMIT_WAIT_RESULT: Option<i64> = std::env::var("QUEUE_LIMIT_WAIT_RESULT")
        .ok()
        .and_then(|x| x.parse().ok());
    pub static ref WAIT_RESULT_FAST_POLL_INTERVAL_MS: u64 = std::env::var("WAIT_RESULT_FAST_POLL_INTERVAL_MS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(50);
    pub static ref WAIT_RESULT_FAST_POLL_DURATION_SECS: u16 = std::env::var("WAIT_RESULT_FAST_POLL_DURATION_SECS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(2);
    pub static ref WAIT_RESULT_SLOW_POLL_INTERVAL_MS: u64 = std::env::var("WAIT_RESULT_SLOW_POLL_INTERVAL_MS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(200);

    static ref JOB_VIEW_AUDIT_LOGS: bool = std::env::var("JOB_VIEW_AUDIT_LOGS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(false);

    static ref JOB_VIEW_CACHE: JobViewCache = JobViewCache::new(50000);
}

struct JobViewCache {
    cache: Cache<String, std::time::Instant>,
}

impl JobViewCache {
    fn new(items_capacity: usize) -> Self {
        Self { cache: Cache::new(items_capacity) }
    }
    fn get_or_insert(&self, key: &str) -> Option<std::time::Instant> {
        match self.cache.get(key) {
            Some(t) if t < std::time::Instant::now() => {
                self.cache.insert(
                    key.to_string(),
                    std::time::Instant::now() + std::time::Duration::from_secs(60),
                );
                None
            }
            v => {
                self.cache.insert(
                    key.to_string(),
                    std::time::Instant::now() + std::time::Duration::from_secs(60),
                );
                v
            }
        }
    }
}

async fn log_job_view(
    db: &DB,
    opt_authed: Option<&ApiAuthed>,
    w_id: &str,
    job_id: &Uuid,
) -> error::Result<()> {
    if *JOB_VIEW_AUDIT_LOGS {
        let audit_author = match opt_authed {
            Some(authed) => AuditAuthor::from(authed),
            None => AuditAuthor {
                username: "anonymous".to_string(),
                username_override: None,
                email: "anonymous".to_string(),
            },
        };
        if JOB_VIEW_CACHE
            .get_or_insert(&format!("{}_{}", job_id, audit_author.email))
            .is_none()
        {
            audit_log(
                db,
                &audit_author,
                "jobs.view",
                ActionKind::Execute,
                w_id,
                Some(&job_id.to_string()),
                None,
            )
            .await?;
        };
    }

    Ok(())
}

pub async fn run_wait_result_job_by_path_get(
    method: hyper::http::Method,
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Extension(db): Extension<DB>,
    Path((w_id, script_path)): Path<(String, StripPath)>,
    Query(run_query): Query<RunJobQuery>,
    DecodeQueries(queries): DecodeQueries,
) -> error::Result<Response> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    if method == http::Method::HEAD {
        return Ok(Json(serde_json::json!("")).into_response());
    }
    let payload_r = run_query.payload.map(decode_payload).map(|x| {
        x.map_err(|e| Error::InternalErr(format!("Impossible to decode query payload: {e:#?}")))
    });

    let mut payload_args = if let Some(payload) = payload_r {
        payload?
    } else {
        HashMap::new()
    };
    queries.iter().for_each(|(k, v)| {
        payload_args.insert(k.to_string(), v.clone());
    });

    let inner_args: HashMap<String, Box<RawValue>> = HashMap::new();
    let args = PushArgs { extra: Some(payload_args), args: &inner_args };

    check_queue_too_long(&db, QUEUE_LIMIT_WAIT_RESULT.or(run_query.queue_limit)).await?;
    let script_path = script_path.to_path();
    check_scopes(&authed, || format!("run:script/{script_path}"))?;

    let mut tx = user_db.clone().begin(&authed).await?;
    let (job_payload, tag, delete_after_use, timeout, on_behalf_authed) =
        script_path_to_payload(script_path, &mut *tx, &w_id, run_query.skip_preprocessor).await?;
    drop(tx);

    let tag = run_query.tag.clone().or(tag);
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;

    let (email, permissioned_as, push_authed, tx) =
        if let Some(on_behalf_of) = on_behalf_authed.as_ref() {
            (
                on_behalf_of.email.as_str(),
                on_behalf_of.permissioned_as.clone(),
                None,
                PushIsolationLevel::IsolatedRoot(db.clone()),
            )
        } else {
            (
                authed.email.as_str(),
                username_to_permissioned_as(&authed.username),
                Some(authed.clone().into()),
                PushIsolationLevel::Isolated(user_db, authed.clone().into()),
            )
        };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        job_payload,
        PushArgs { args: &args.args, extra: args.extra },
        authed.display_username(),
        email,
        permissioned_as,
        None,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        timeout,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;

    let wait_result = run_wait_result(&db, uuid, w_id, None, &authed.username).await;
    if delete_after_use.unwrap_or(false) {
        delete_job_metadata_after_use(&db, uuid).await?;
    }
    return wait_result;
}

pub async fn run_wait_result_flow_by_path_get(
    method: hyper::http::Method,
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Extension(db): Extension<DB>,
    Path((w_id, flow_path)): Path<(String, StripPath)>,
    Query(run_query): Query<RunJobQuery>,
    DecodeQueries(queries): DecodeQueries,
) -> error::Result<Response> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    if method == http::Method::HEAD {
        return Ok(Json(serde_json::json!("")).into_response());
    }
    let payload_r = run_query.payload.clone().map(decode_payload).map(|x| {
        x.map_err(|e| {
            error::Error::InternalErr(format!("Impossible to decode query payload: {e:#?}"))
        })
    });

    let mut payload_args = if let Some(payload) = payload_r {
        payload?
    } else {
        HashMap::new()
    };

    queries.iter().for_each(|(k, v)| {
        payload_args.insert(k.to_string(), v.clone());
    });

    let args = PushArgsOwned { extra: Some(payload_args), args: HashMap::new() };

    run_wait_result_flow_by_path_internal(
        db, run_query, flow_path, authed, user_db, args, w_id, None,
    )
    .await
}

pub async fn run_wait_result_script_by_path(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Extension(db): Extension<DB>,
    Path((w_id, script_path)): Path<(String, StripPath)>,
    Query(run_query): Query<RunJobQuery>,
    args: WebhookArgs,
) -> error::Result<Response> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    let args = args.to_push_args_owned(&authed, &db, &w_id).await?;

    run_wait_result_script_by_path_internal(
        db,
        run_query,
        script_path,
        authed,
        user_db,
        w_id,
        args,
        None,
    )
    .await
}

pub async fn run_wait_result_script_by_path_internal(
    db: sqlx::Pool<Postgres>,
    run_query: RunJobQuery,
    script_path: StripPath,
    authed: ApiAuthed,
    user_db: UserDB,
    w_id: String,
    args: PushArgsOwned,
    label_prefix: Option<String>,
) -> error::Result<Response> {
    check_queue_too_long(&db, QUEUE_LIMIT_WAIT_RESULT.or(run_query.queue_limit)).await?;
    let script_path = script_path.to_path();
    check_scopes(&authed, || format!("run:script/{script_path}"))?;

    let mut tx = user_db.clone().begin(&authed).await?;
    let (job_payload, tag, delete_after_use, timeout, on_behalf_of) =
        script_path_to_payload(script_path, &mut *tx, &w_id, run_query.skip_preprocessor).await?;

    let tag = run_query.tag.clone().or(tag);
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;

    let (email, permissioned_as, push_authed, tx) =
        if let Some(on_behalf_of) = on_behalf_of.as_ref() {
            (
                on_behalf_of.email.as_str(),
                on_behalf_of.permissioned_as.clone(),
                None,
                PushIsolationLevel::IsolatedRoot(db.clone()),
            )
        } else {
            (
                authed.email.as_str(),
                username_to_permissioned_as(&authed.username),
                Some(authed.clone().into()),
                PushIsolationLevel::Isolated(user_db, authed.clone().into()),
            )
        };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        job_payload,
        PushArgs { args: &args.args, extra: args.extra },
        &label_prefix
            .map(|x| x + authed.display_username())
            .unwrap_or_else(|| authed.display_username().to_string()),
        email,
        permissioned_as,
        None,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        timeout,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;

    let wait_result = run_wait_result(&db, uuid, w_id, None, &authed.username).await;
    if delete_after_use.unwrap_or(false) {
        delete_job_metadata_after_use(&db, uuid).await?;
    }
    return wait_result;
}

pub async fn run_wait_result_script_by_hash(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Extension(db): Extension<DB>,
    Path((w_id, script_hash)): Path<(String, ScriptHash)>,
    Query(run_query): Query<RunJobQuery>,
    args: WebhookArgs,
) -> error::Result<Response> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    let args = args.to_push_args_owned(&authed, &db, &w_id).await?;

    check_queue_too_long(&db, run_query.queue_limit).await?;

    let hash = script_hash.0;
    let (
        path,
        tag,
        custom_concurrency_key,
        concurrent_limit,
        concurrency_time_window_s,
        mut cache_ttl,
        language,
        dedicated_worker,
        priority,
        delete_after_use,
        timeout,
        has_preprocessor,
        on_behalf_of_email,
        created_by,
    ) = get_path_tag_limits_cache_for_hash(user_db.clone().begin(&authed).await?, &w_id, hash)
        .await?;
    if let Some(run_query_cache_ttl) = run_query.cache_ttl {
        cache_ttl = Some(run_query_cache_ttl);
    }
    check_scopes(&authed, || format!("run:script/{path}"))?;

    let tag = run_query.tag.clone().or(tag);
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;

    let (email, permissioned_as, push_authed, tx) = if let Some(email) = on_behalf_of_email.as_ref()
    {
        (
            email,
            username_to_permissioned_as(created_by.as_str()),
            None,
            PushIsolationLevel::IsolatedRoot(db.clone()),
        )
    } else {
        (
            &authed.email,
            username_to_permissioned_as(&authed.username),
            Some(authed.clone().into()),
            PushIsolationLevel::Isolated(user_db, authed.clone().into()),
        )
    };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        JobPayload::ScriptHash {
            hash: ScriptHash(hash),
            path: path,
            custom_concurrency_key,
            concurrent_limit: concurrent_limit,
            concurrency_time_window_s: concurrency_time_window_s,
            cache_ttl,
            language,
            dedicated_worker,
            priority,
            apply_preprocessor: !run_query.skip_preprocessor.unwrap_or(false)
                && has_preprocessor.unwrap_or(false),
        },
        PushArgs { args: &args.args, extra: args.extra },
        authed.display_username(),
        email,
        permissioned_as,
        None,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        timeout,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;

    let wait_result = run_wait_result(&db, uuid, w_id, None, &authed.username).await;
    if delete_after_use.unwrap_or(false) {
        delete_job_metadata_after_use(&db, uuid).await?;
    }
    return wait_result;
}

pub async fn run_wait_result_flow_by_path(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Extension(db): Extension<DB>,
    Path((w_id, flow_path)): Path<(String, StripPath)>,
    Query(run_query): Query<RunJobQuery>,
    args: WebhookArgs,
) -> error::Result<Response> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    let args = args.to_push_args_owned(&authed, &db, &w_id).await?;

    run_wait_result_flow_by_path_internal(
        db, run_query, flow_path, authed, user_db, args, w_id, None,
    )
    .await
}

pub async fn run_wait_result_flow_by_path_internal(
    db: sqlx::Pool<Postgres>,
    run_query: RunJobQuery,
    flow_path: StripPath,
    authed: ApiAuthed,
    user_db: UserDB,
    args: PushArgsOwned,
    w_id: String,
    label_prefix: Option<String>,
) -> error::Result<Response> {
    check_queue_too_long(&db, run_query.queue_limit).await?;

    let flow_path = flow_path.to_path();
    check_scopes(&authed, || format!("run:flow/{flow_path}"))?;

    let scheduled_for = run_query.get_scheduled_for(&db).await?;

    let mut tx = user_db.clone().begin(&authed).await?;
    let (tag, dedicated_worker, early_return, has_preprocessor, on_behalf_of_email, edited_by) = sqlx::query!(
        "SELECT tag, dedicated_worker, flow_version.value->>'early_return' as early_return, flow_version.value->>'preprocessor_module' IS NOT NULL as has_preprocessor, on_behalf_of_email, edited_by
        FROM flow 
        LEFT JOIN flow_version
            ON flow_version.id = flow.versions[array_upper(flow.versions, 1)]
        WHERE flow.path = $1 and flow.workspace_id = $2",
        flow_path,
        w_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .map(|x| (x.tag, x.dedicated_worker, x.early_return, x.has_preprocessor, x.on_behalf_of_email, x.edited_by))
    .ok_or_else(|| {
        Error::NotFound(format!(
            "flow not found at path {flow_path} in workspace {w_id}"
        ))
    })?;

    let tag = run_query.tag.clone().or(tag);
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;

    let (email, permissioned_as, push_authed, tx) =
        if let Some(on_behalf_of_email) = on_behalf_of_email.as_ref() {
            (
                on_behalf_of_email,
                username_to_permissioned_as(&edited_by),
                None,
                PushIsolationLevel::IsolatedRoot(db.clone()),
            )
        } else {
            (
                &authed.email,
                username_to_permissioned_as(&authed.username),
                Some(authed.clone().into()),
                PushIsolationLevel::Isolated(user_db, authed.clone().into()),
            )
        };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        JobPayload::Flow {
            path: flow_path.to_string(),
            dedicated_worker,
            apply_preprocessor: !run_query.skip_preprocessor.unwrap_or(false)
                && has_preprocessor.unwrap_or(false),
        },
        PushArgs { args: &args.args, extra: args.extra },
        &label_prefix
            .map(|x| x + authed.display_username())
            .unwrap_or_else(|| authed.display_username().to_string()),
        email,
        permissioned_as,
        scheduled_for,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        None,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;

    run_wait_result(&db, uuid, w_id, early_return, &authed.username).await
}

async fn run_preview_script(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(run_query): Query<RunJobQuery>,
    Json(preview): Json<Preview>,
) -> error::Result<(StatusCode, String)> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    check_scopes(&authed, || format!("jobs:runscript"))?;
    if authed.is_operator {
        return Err(error::Error::NotAuthorized(
            "Operators cannot run preview jobs for security reasons".to_string(),
        ));
    }
    let scheduled_for = run_query.get_scheduled_for(&db).await?;
    let tag = run_query.tag.clone().or(preview.tag.clone());
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;
    let tx = PushIsolationLevel::Isolated(user_db.clone(), authed.clone().into());

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        match preview.kind {
            Some(PreviewKind::Identity) => JobPayload::Identity,
            Some(PreviewKind::Noop) => JobPayload::Noop,
            _ => JobPayload::Code(RawCode {
                hash: None,
                content: preview.content.unwrap_or_default(),
                path: preview.path,
                language: preview.language.unwrap_or(ScriptLang::Deno),
                lock: preview.lock,
                custom_concurrency_key: None,
                concurrent_limit: None, // TODO(gbouv): once I find out how to store limits in the content of a script, should be easy to plug limits here
                concurrency_time_window_s: None, // TODO(gbouv): same as above
                cache_ttl: None,
                dedicated_worker: preview.dedicated_worker,
            }),
        },
        PushArgs::from(&preview.args.unwrap_or_default()),
        authed.display_username(),
        &authed.email,
        username_to_permissioned_as(&authed.username),
        scheduled_for,
        None,
        None,
        None,
        run_query.job_id,
        false,
        false,
        None,
        true,
        tag,
        run_query.timeout,
        None,
        None,
        Some(&authed.clone().into()),
    )
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, uuid.to_string()))
}

#[cfg(all(feature = "enterprise", feature = "parquet"))]
async fn run_bundle_preview_script(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(run_query): Query<RunJobQuery>,
    mut multipart: axum::extract::Multipart,
) -> error::Result<(StatusCode, String)> {
    use windmill_common::scripts::PREVIEW_IS_TAR_CODEBASE_HASH;

    check_license_key_valid().await?;

    check_scopes(&authed, || format!("jobs:runscript"))?;
    if authed.is_operator {
        return Err(error::Error::NotAuthorized(
            "Operators cannot run preview jobs for security reasons".to_string(),
        ));
    }

    let mut job_id = None;
    let mut tx = None;
    let mut uploaded = false;
    let mut is_tar = false;

    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.name().unwrap().to_string();
        let data = field.bytes().await;
        let data = data.map_err(to_anyhow)?;
        if name == "preview" {
            let preview: Preview = serde_json::from_slice(&data).map_err(to_anyhow)?;

            let scheduled_for = run_query.get_scheduled_for(&db).await?;
            let tag = run_query.tag.clone().or(preview.tag.clone());
            check_tag_available_for_workspace(&w_id, &tag, &authed).await?;
            let ltx = PushIsolationLevel::Isolated(user_db.clone(), authed.clone().into());

            let args = preview.args.unwrap_or_default();

            is_tar = match preview.kind {
                Some(PreviewKind::Tarbundle) => true,
                _ => false,
            };

            // tracing::info!("is_tar 1: {is_tar}");
            // hmap.insert("")
            let (uuid, ntx) = push(
                &db,
                ltx,
                &w_id,
                JobPayload::Code(RawCode {
                    hash: if is_tar {
                        Some(PREVIEW_IS_TAR_CODEBASE_HASH)
                    } else {
                        Some(PREVIEW_IS_CODEBASE_HASH)
                    },
                    content: preview.content.unwrap_or_default(),
                    path: preview.path,
                    language: preview.language.unwrap_or(ScriptLang::Deno),
                    lock: preview.lock,
                    concurrent_limit: None, // TODO(gbouv): once I find out how to store limits in the content of a script, should be easy to plug limits here
                    concurrency_time_window_s: None, // TODO(gbouv): same as above
                    cache_ttl: None,
                    dedicated_worker: preview.dedicated_worker,
                    custom_concurrency_key: None,
                }),
                PushArgs::from(&args),
                authed.display_username(),
                &authed.email,
                username_to_permissioned_as(&authed.username),
                scheduled_for,
                None,
                None,
                None,
                run_query.job_id,
                false,
                false,
                None,
                true,
                tag,
                run_query.timeout,
                None,
                None,
                Some(&authed.clone().into()),
            )
            .await?;
            job_id = Some(uuid);
            tx = Some(ntx);
        }
        if name == "file" {
            let mut id = job_id
                .as_ref()
                .ok_or_else(|| {
                    Error::BadRequest(
                        "script need to be passed first in the multipart upload".to_string(),
                    )
                })?
                .to_string();

            // tracing::info!("is_tar 2: {is_tar}");

            if is_tar {
                id = format!("{}.tar", id);
            }

            uploaded = true;

            if let Some(os) = windmill_common::s3_helpers::OBJECT_STORE_CACHE_SETTINGS
                .read()
                .await
                .clone()
            {
                let path = windmill_common::s3_helpers::bundle(&w_id, &id);
                if let Err(e) = os
                    .put(&object_store::path::Path::from(path.clone()), data.into())
                    .await
                {
                    tracing::info!("Failed to put snapshot to s3 at {path}: {:?}", e);
                    return Err(Error::ExecutionErr(format!("Failed to put {path} to s3")));
                }
            } else {
                return Err(Error::BadConfig("Object store is required for snapshot script and is not configured for servers".to_string()));
            }
        }
        // println!("Length of `{}` is {} bytes", name, data.len());
    }
    if !uploaded {
        return Err(Error::BadRequest("No file uploaded".to_string()));
    }
    if job_id.is_none() {
        return Err(Error::BadRequest(
            "No script found in the uploaded file".to_string(),
        ));
    }

    tx.unwrap().commit().await?;

    Ok((StatusCode::CREATED, job_id.unwrap().to_string()))
}

#[cfg(not(all(feature = "enterprise", feature = "parquet")))]
async fn run_bundle_preview_script() -> error::Result<(StatusCode, String)> {
    return Err(Error::BadRequest(
        "bundle preview is an ee feature".to_string(),
    ));
}

#[derive(Deserialize)]
pub struct RunDependenciesRequest {
    pub raw_scripts: Vec<RawScriptForDependencies>,
    pub entrypoint: String,
    pub raw_deps: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct RawScriptForDependencies {
    pub script_path: String,
    pub raw_code: Option<String>,
    pub language: ScriptLang,
}

#[derive(Serialize)]
pub struct RunDependenciesResponse {
    pub dependencies: String,
}

async fn run_dependencies_job(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Path(w_id): Path<String>,
    Json(req): Json<RunDependenciesRequest>,
) -> error::Result<Response> {
    if authed.is_operator {
        return Err(error::Error::NotAuthorized(
            "Operators cannot run dependencies jobs for security reasons".to_string(),
        ));
    }

    if req.raw_scripts.len() != 1 || req.raw_scripts[0].script_path != req.entrypoint {
        return Err(error::Error::InternalErr(
            "For now only a single raw script can be passed to this endpoint, and the entrypoint should be set to the script path".to_string(),
        ));
    }
    let raw_script = req.raw_scripts[0].clone();
    let script_path = raw_script.script_path;
    let ehm = HashMap::new();
    let raw_code = raw_script.raw_code.unwrap_or_else(|| "".to_string());
    let language = raw_script.language;

    let (args, raw_code) = if let Some(deps) = req.raw_deps {
        let mut hm = HashMap::new();
        hm.insert(
            "raw_deps".to_string(),
            JsonRawValue::from_string("true".to_string()).unwrap(),
        );
        if language == ScriptLang::Bun {
            let annotation = windmill_common::worker::TypeScriptAnnotations::parse(&raw_code);
            hm.insert(
                "npm_mode".to_string(),
                JsonRawValue::from_string(annotation.npm.to_string()).unwrap(),
            );
        }
        (PushArgs { extra: Some(hm), args: &ehm }, deps)
    } else {
        (PushArgs::from(&ehm), raw_code)
    };

    let (uuid, tx) = push(
        &db,
        PushIsolationLevel::IsolatedRoot(db.clone()),
        &w_id,
        JobPayload::RawScriptDependencies {
            script_path: script_path,
            content: raw_code,
            language: language,
        },
        args,
        authed.display_username(),
        &authed.email,
        username_to_permissioned_as(&authed.username),
        None,
        None,
        None,
        None,
        None,
        false,
        false,
        None,
        true,
        None,
        None,
        None,
        None,
        Some(&authed.clone().into()),
    )
    .await?;
    tx.commit().await?;

    let wait_result = run_wait_result(&db, uuid, w_id, None, &authed.username).await;
    wait_result
}

#[derive(Deserialize)]
pub struct RunFlowDependenciesRequest {
    pub path: String,
    pub flow_value: FlowValue,
}

#[derive(Serialize)]
pub struct RunFlowDependenciesResponse {
    pub dependencies: String,
}

async fn run_flow_dependencies_job(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Path(w_id): Path<String>,
    Json(req): Json<RunFlowDependenciesRequest>,
) -> error::Result<Response> {
    if authed.is_operator {
        return Err(error::Error::NotAuthorized(
            "Operators cannot run dependencies jobs for security reasons".to_string(),
        ));
    }

    let (uuid, tx) = push(
        &db,
        PushIsolationLevel::IsolatedRoot(db.clone()),
        &w_id,
        JobPayload::RawFlowDependencies { path: req.path, flow_value: req.flow_value },
        PushArgs::from(&HashMap::from([(
            "skip_flow_update".to_string(),
            to_raw_value(&true),
        )])),
        authed.display_username(),
        &authed.email,
        username_to_permissioned_as(&authed.username),
        None,
        None,
        None,
        None,
        None,
        false,
        false,
        None,
        true,
        None,
        None,
        None,
        None,
        Some(&authed.clone().into()),
    )
    .await?;
    tx.commit().await?;

    let wait_result = run_wait_result(&db, uuid, w_id, None, &authed.username).await;
    wait_result
}

#[derive(Deserialize)]
struct BatchRawScript {
    content: String,
    language: Option<ScriptLang>,
    lock: Option<String>,
}

#[derive(Deserialize)]
struct BatchInfo {
    kind: String,
    flow_value: Option<FlowValue>,
    path: Option<String>,
    rawscript: Option<BatchRawScript>,
}

#[tracing::instrument(level = "trace", skip_all)]
async fn add_batch_jobs(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, n)): Path<(String, i32)>,
    Json(batch_info): Json<BatchInfo>,
) -> error::JsonResult<Vec<Uuid>> {
    require_super_admin(&db, &authed.email).await?;

    let (
        hash,
        path,
        job_kind,
        language,
        dedicated_worker,
        custom_concurrency_key,
        concurrent_limit,
        concurrent_time_window_s,
        timeout,
        raw_code,
        raw_lock,
        raw_flow,
        flow_status,
    ) = match batch_info.kind.as_str() {
        "script" => {
            if let Some(path) = batch_info.path {
                let mut tx = user_db.clone().begin(&authed).await?;
                let (
                    script_hash,
                    _tag,
                    custom_concurrency_key,
                    concurrent_limit,
                    concurrency_time_window_s,
                    _cache_ttl,
                    language,
                    dedicated_worker,
                    _priority,
                    _delete_after_use,
                    timeout,
                    _,
                    _, // TODO: consider on_behalf_of_email and created_by for batch jobs
                    _, // ------------------------------------------
                ) = get_latest_deployed_hash_for_path(&mut *tx, &w_id, &path).await?;
                (
                    Some(script_hash),
                    Some(path),
                    JobKind::Script,
                    Some(language),
                    dedicated_worker,
                    custom_concurrency_key,
                    concurrent_limit,
                    concurrency_time_window_s,
                    timeout,
                    None,
                    None,
                    None,
                    None,
                )
            } else {
                Err(anyhow::anyhow!(
                    "Path is required if no value is not provided"
                ))?
            }
        }
        "rawscript" => {
            if let Some(rawscript) = batch_info.rawscript {
                (
                    None,
                    None,
                    JobKind::Preview,
                    rawscript.language,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(rawscript.content),
                    rawscript.lock,
                    None,
                    None,
                )
            } else {
                Err(anyhow::anyhow!(
                    "rawscript is required for `rawscript` kind"
                ))?
            }
        }
        "flow" => {
            let (mut value, job_kind, path) = if let Some(value) = batch_info.flow_value {
                (value, JobKind::FlowPreview, None)
            } else if let Some(path) = batch_info.path {
                let mut tx = user_db.clone().begin(&authed).await?;
                let value_json = sqlx::query!(
                    "SELECT coalesce(flow_version_lite.value, flow_version.value) as \"value!: sqlx::types::Json<Box<RawValue>>\" FROM flow 
                    LEFT JOIN flow_version
                        ON flow_version.id = flow.versions[array_upper(flow.versions, 1)]
                    LEFT JOIN flow_version_lite 
                        ON flow_version_lite.id = flow_version.id
                    WHERE flow.path = $1 AND flow.workspace_id = $2 LIMIT 1",
                    &path, &w_id
                )
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| Error::InternalErr(format!("not found flow at path {:?}", path)))?;
                let value =
                    serde_json::from_str::<FlowValue>(value_json.value.get()).map_err(|err| {
                        Error::InternalErr(format!(
                            "could not convert json to flow for {path}: {err:?}"
                        ))
                    })?;
                (value, JobKind::Flow, Some(path))
            } else {
                Err(anyhow::anyhow!(
                    "Path is required if no value is not provided"
                ))?
            };
            add_virtual_items_if_necessary(&mut value.modules);
            let flow_status = FlowStatus::new(&value);
            (
                None,                            // script_hash
                path,                            // script_path
                job_kind,                        // job_kind
                None,                            // language
                None,                            // dedicated_worker
                value.concurrency_key.clone(),   // custom_concurrency_key
                value.concurrent_limit.clone(),  // concurrent_limit
                value.concurrency_time_window_s, // concurrency_time_window_s
                None,                            // timeout
                None,                            // raw_code
                None,                            // raw_lock
                Some(value),                     // raw_flow
                Some(flow_status),               // flow_status
            )
        }
        "noop" => (
            None,
            None,
            JobKind::Noop,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        _ => {
            return Err(error::Error::BadRequest(format!(
                "Invalid batch kind: {}",
                batch_info.kind
            )))
        }
    };

    let language = language.unwrap_or(ScriptLang::Deno);

    let tag = if let Some(dedicated_worker) = dedicated_worker {
        if dedicated_worker && path.is_some() {
            format!("{}:{}", w_id, path.clone().unwrap())
        } else {
            format!("{}", language.as_str())
        }
    } else {
        format!("{}", language.as_str())
    };

    let mut tx = user_db.begin(&authed).await?;

    let uuids = sqlx::query_scalar!(
        r#"WITH uuid_table as (
            select gen_random_uuid() as uuid from generate_series(1, $5)
        )
        INSERT INTO job
            (id, workspace_id, raw_code, raw_lock, raw_flow)
            (SELECT uuid, $1, $2, $3, $4 FROM uuid_table)
        RETURNING id"#,
        w_id,
        raw_code,
        raw_lock,
        raw_flow.map(sqlx::types::Json) as Option<sqlx::types::Json<FlowValue>>,
        n
    )
    .fetch_all(&mut *tx)
    .await?;

    let uuids = sqlx::query_scalar!(
        r#"WITH uuid_table as (
            select unnest($11::uuid[]) as uuid
        )
        INSERT INTO queue 
            (id, script_hash, script_path, job_kind, language, args, tag, created_by, permissioned_as, email, scheduled_for, workspace_id, concurrent_limit, concurrency_time_window_s, timeout, flow_status)
            (SELECT uuid, $1, $2, $3, $4, ('{ "uuid": "' || uuid || '" }')::jsonb, $5, $6, $7, $8, $9, $10, $12, $13, $14, $15 FROM uuid_table) 
        RETURNING id"#,
            hash.map(|h| h.0),
            path,
            job_kind.clone() as JobKind,
            language as ScriptLang,
            tag,
            authed.username,
            username_to_permissioned_as(&authed.username),
            authed.email,
            Utc::now(),
            w_id,
            &uuids,
            concurrent_limit,
            concurrent_time_window_s,
            timeout,
            flow_status.map(sqlx::types::Json) as Option<sqlx::types::Json<FlowStatus>>
        )
        .fetch_all(&mut *tx)
        .await?;

    if let Some(custom_concurrency_key) = custom_concurrency_key {
        sqlx::query!(
            "INSERT INTO concurrency_key (job_id, key) SELECT id, $1 FROM unnest($2::uuid[]) as id",
            custom_concurrency_key,
            &uuids
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(Json(uuids))
}

async fn run_preview_flow_job(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(run_query): Query<RunJobQuery>,
    Json(raw_flow): Json<PreviewFlow>,
) -> error::Result<(StatusCode, String)> {
    check_scopes(&authed, || format!("jobs:runflow"))?;
    if authed.is_operator {
        return Err(error::Error::NotAuthorized(
            "Operators cannot run preview jobs for security reasons".to_string(),
        ));
    }
    let scheduled_for = run_query.get_scheduled_for(&db).await?;
    let tag = run_query.tag.clone().or(raw_flow.tag.clone());
    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;
    let tx = PushIsolationLevel::Isolated(user_db.clone(), authed.clone().into());

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        JobPayload::RawFlow {
            value: raw_flow.value,
            path: raw_flow.path,
            restarted_from: raw_flow.restarted_from,
        },
        PushArgs::from(&raw_flow.args.unwrap_or_default()),
        authed.display_username(),
        &authed.email,
        username_to_permissioned_as(&authed.username),
        scheduled_for,
        None,
        None,
        None,
        run_query.job_id,
        false,
        false,
        None,
        true,
        tag,
        None,
        None,
        None,
        Some(&authed.clone().into()),
    )
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, uuid.to_string()))
}

pub async fn run_job_by_hash(
    authed: ApiAuthed,
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, script_hash)): Path<(String, ScriptHash)>,
    Query(run_query): Query<RunJobQuery>,
    args: WebhookArgs,
) -> error::Result<(StatusCode, String)> {
    let args = args.to_push_args_owned(&authed, &db, &w_id).await?;
    run_job_by_hash_inner(
        authed,
        db,
        user_db,
        w_id,
        script_hash,
        run_query,
        args,
        None,
    )
    .await
}

pub async fn run_job_by_hash_inner(
    authed: ApiAuthed,
    db: DB,
    user_db: UserDB,
    w_id: String,
    script_hash: ScriptHash,
    run_query: RunJobQuery,
    args: PushArgsOwned,
    label_prefix: Option<String>,
) -> error::Result<(StatusCode, String)> {
    #[cfg(feature = "enterprise")]
    check_license_key_valid().await?;

    let hash = script_hash.0;
    let (
        path,
        tag,
        custom_concurrency_key,
        concurrent_limit,
        concurrency_time_window_s,
        mut cache_ttl,
        language,
        dedicated_worker,
        priority,
        _delete_after_use, // not taken into account in async endpoints
        timeout,
        has_preprocessor,
        on_behalf_of_email,
        created_by,
    ) = get_path_tag_limits_cache_for_hash(user_db.clone().begin(&authed).await?, &w_id, hash)
        .await?;
    check_scopes(&authed, || format!("run:script/{path}"))?;
    if let Some(run_query_cache_ttl) = run_query.cache_ttl {
        cache_ttl = Some(run_query_cache_ttl);
    }
    let scheduled_for = run_query.get_scheduled_for(&db).await?;
    let tag = run_query.tag.clone().or(tag);

    check_tag_available_for_workspace(&w_id, &tag, &authed).await?;

    let (email, permissioned_as, push_authed, tx) = if let Some(email) = on_behalf_of_email.as_ref()
    {
        (
            email,
            username_to_permissioned_as(created_by.as_str()),
            None,
            PushIsolationLevel::IsolatedRoot(db.clone()),
        )
    } else {
        (
            &authed.email,
            username_to_permissioned_as(&authed.username),
            Some(authed.clone().into()),
            PushIsolationLevel::Isolated(user_db, authed.clone().into()),
        )
    };

    let (uuid, tx) = push(
        &db,
        tx,
        &w_id,
        JobPayload::ScriptHash {
            hash: ScriptHash(hash),
            path: path,
            custom_concurrency_key,
            concurrent_limit: concurrent_limit,
            concurrency_time_window_s: concurrency_time_window_s,
            cache_ttl,
            language,
            dedicated_worker,
            priority,
            apply_preprocessor: !run_query.skip_preprocessor.unwrap_or(false)
                && has_preprocessor.unwrap_or(false),
        },
        PushArgs { args: &args.args, extra: args.extra },
        &label_prefix
            .map(|x| x + authed.display_username())
            .unwrap_or_else(|| authed.display_username().to_string()),
        email,
        permissioned_as,
        scheduled_for,
        None,
        run_query.parent_job,
        run_query.root_job.or(run_query.parent_job),
        run_query.job_id,
        false,
        false,
        None,
        !run_query.invisible_to_owner.unwrap_or(false),
        tag,
        timeout,
        None,
        None,
        push_authed.as_ref(),
    )
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, uuid.to_string()))
}

#[derive(Deserialize)]
pub struct JobUpdateQuery {
    pub running: bool,
    pub log_offset: i32,
    pub get_progress: Option<bool>,
}

#[derive(Serialize)]
pub struct JobUpdate {
    pub running: Option<bool>,
    pub completed: Option<bool>,
    pub new_logs: Option<String>,
    pub log_offset: Option<i32>,
    pub mem_peak: Option<i32>,
    pub progress: Option<i32>,
    pub flow_status: Option<Box<serde_json::value::RawValue>>,
}

async fn get_log_file(Path((_w_id, file_p)): Path<(String, String)>) -> error::Result<Response> {
    let local_file = format!("{TMP_DIR}/logs/{file_p}");
    if tokio::fs::metadata(&local_file).await.is_ok() {
        let mut file = tokio::fs::File::open(local_file).await.map_err(to_anyhow)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).await.map_err(to_anyhow)?;
        let res = Response::builder()
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(Body::from(bytes::Bytes::from(buffer)))
            .unwrap();
        return Ok(res);
    }

    #[cfg(all(feature = "enterprise", feature = "parquet"))]
    if let Some(os) = OBJECT_STORE_CACHE_SETTINGS.read().await.clone() {
        let file = os
            .get(&object_store::path::Path::from(format!("logs/{file_p}")))
            .await;
        if let Ok(file) = file {
            if let Ok(bytes) = file.bytes().await {
                use axum::http::header;
                let res = Response::builder()
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(bytes::Bytes::from(bytes)))
                    .unwrap();
                return Ok(res);
            } else {
                return Err(error::Error::InternalErr(format!(
                    "Error getting bytes from file: {}",
                    file_p
                )));
            }
        } else {
            return Err(error::Error::NotFound(format!(
                "File not found: {}",
                file_p
            )));
        }
    } else {
        return Err(error::Error::InternalErr(format!(
            "Object store client not present and file not found on server logs volume at {local_file}"
        )));
    }

    #[cfg(not(all(feature = "enterprise", feature = "parquet")))]
    return Err(error::Error::NotFound(format!(
        "File not found on server logs volume /tmp/windmill/logs and no distributed logs s3 storage for {}",
        file_p
    )));
}

#[derive(Deserialize, sqlx::FromRow)]
pub struct JobUpdateRow {
    pub running: bool,
    pub logs: Option<String>,
    pub mem_peak: Option<i32>,
    pub flow_status: Option<sqlx::types::Json<Box<serde_json::value::RawValue>>>,
    pub log_offset: Option<i32>,
    pub created_by: String,
}
async fn get_job_update(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, job_id)): Path<(String, Uuid)>,
    Query(JobUpdateQuery { running, log_offset, get_progress }): Query<JobUpdateQuery>,
) -> error::JsonResult<JobUpdate> {
    let record = sqlx::query_as::<_, JobUpdateRow>(
        "SELECT running, substr(concat(coalesce(queue.logs, ''), job_logs.logs), greatest($1 - job_logs.log_offset, 0)) as logs, mem_peak, 
        CASE WHEN is_flow_step is true then NULL else flow_status END as flow_status,
        job_logs.log_offset + char_length(job_logs.logs) + 1 as log_offset, created_by
        FROM queue
        LEFT JOIN job_logs ON job_logs.job_id =  queue.id 
        WHERE queue.workspace_id = $2 AND queue.id = $3",
    )
    .bind(log_offset)
    .bind(&w_id)
    .bind(&job_id)
    .fetch_optional(&db)
    .await?;

    let progress: Option<i32> = if get_progress == Some(true) {
        sqlx::query_scalar!(
                "SELECT scalar_int FROM job_stats WHERE workspace_id = $1 AND job_id = $2 AND metric_id = $3",
                &w_id,
                 job_id,
                "progress_perc"
            )
            .fetch_optional(&db)
            .await?.and_then(|inner| inner)
    } else {
        None
    };

    if let Some(record) = record {
        if opt_authed.is_none() && record.created_by != "anonymous" {
            return Err(Error::BadRequest(
                "As a non logged in user, you can only see jobs ran by anonymous users".to_string(),
            ));
        }
        log_job_view(&db, opt_authed.as_ref(), &w_id, &job_id).await?;
        Ok(Json(JobUpdate {
            running: if !running && record.running {
                Some(true)
            } else {
                None
            },
            log_offset: record.log_offset,
            completed: None,
            new_logs: record.logs,
            mem_peak: record.mem_peak,
            progress,
            flow_status: record
                .flow_status
                .map(|x: sqlx::types::Json<Box<RawValue>>| x.0),
        }))
    } else {
        let record = sqlx::query_as::<_, JobUpdateRow>(
            "SELECT false as running, substr(concat(coalesce(completed_job.logs, ''), job_logs.logs), greatest($1 - job_logs.log_offset, 0))  as logs, mem_peak, 
            CASE WHEN is_flow_step is true then NULL else flow_status END as flow_status,
            job_logs.log_offset + char_length(job_logs.logs) + 1 as log_offset, created_by
            FROM completed_job 
            LEFT JOIN job_logs ON job_logs.job_id = completed_job.id 
            WHERE completed_job.workspace_id = $2 AND id = $3",
        )
        .bind(log_offset)
        .bind(&w_id)
        .bind(&job_id)
        .fetch_optional(&db)
        .await?;
        if let Some(record) = record {
            if opt_authed.is_none() && record.created_by != "anonymous" {
                return Err(Error::BadRequest(
                    "As a non logged in user, you can only see jobs ran by anonymous users"
                        .to_string(),
                ));
            }
            log_job_view(&db, opt_authed.as_ref(), &w_id, &job_id).await?;
            Ok(Json(JobUpdate {
                running: Some(false),
                completed: Some(true),
                log_offset: record.log_offset,
                new_logs: record.logs,
                mem_peak: record.mem_peak,
                progress,
                flow_status: record
                    .flow_status
                    .map(|x: sqlx::types::Json<Box<RawValue>>| x.0),
            }))
        } else {
            Err(error::Error::NotFound(format!("Job not found: {}", job_id)))
        }
    }
}

pub fn filter_list_completed_query(
    mut sqlb: SqlBuilder,
    lq: &ListCompletedQuery,
    w_id: &str,
    join_outstanding_wait_times: bool,
) -> SqlBuilder {
    if join_outstanding_wait_times {
        sqlb.left()
            .join("outstanding_wait_time")
            .on_eq("id", "outstanding_wait_time.job_id");
    }

    if let Some(label) = &lq.label {
        let mut wh = format!("result->'wm_labels' ? ");
        wh.push_str(&format!("'{}'", &label.replace("'", "''")));
        sqlb.and_where("result ? 'wm_labels'");
        sqlb.and_where(&wh);
    }

    if w_id != "admins" || !lq.all_workspaces.is_some_and(|x| x) {
        sqlb.and_where_eq("workspace_id", "?".bind(&w_id));
    }

    if let Some(p) = &lq.schedule_path {
        sqlb.and_where_eq("schedule_path", "?".bind(p));
    }

    if let Some(ps) = &lq.script_path_start {
        sqlb.and_where_like_left("script_path", ps);
    }
    if let Some(p) = &lq.script_path_exact {
        sqlb.and_where_eq("script_path", "?".bind(p));
    }
    if let Some(h) = &lq.script_hash {
        sqlb.and_where_eq("script_hash", "?".bind(h));
    }
    if let Some(t) = &lq.tag {
        sqlb.and_where_eq("tag", "?".bind(t));
    }
    if let Some(cb) = &lq.created_by {
        sqlb.and_where_eq("created_by", "?".bind(cb));
    }
    if let Some(r) = &lq.success {
        sqlb.and_where_eq("success", r);
    }
    if let Some(pj) = &lq.parent_job {
        sqlb.and_where_eq("parent_job", "?".bind(pj));
    }
    if let Some(dt) = &lq.started_before {
        sqlb.and_where_le("started_at", "?".bind(&dt.to_rfc3339()));
    }
    if let Some(dt) = &lq.started_after {
        sqlb.and_where_ge("started_at", "?".bind(&dt.to_rfc3339()));
    }

    if let Some(dt) = &lq.created_or_started_before {
        sqlb.and_where_le("started_at", "?".bind(&dt.to_rfc3339()));
    }
    if let Some(dt) = &lq.created_or_started_after {
        sqlb.and_where_ge("started_at", "?".bind(&dt.to_rfc3339()));
    }

    if let Some(dt) = &lq.created_before {
        sqlb.and_where_le("created_at", "?".bind(&dt.to_rfc3339()));
    }
    if let Some(dt) = &lq.created_after {
        sqlb.and_where_ge("created_at", "?".bind(&dt.to_rfc3339()));
    }

    if let Some(dt) = &lq.created_or_started_after_completed_jobs {
        sqlb.and_where_ge("started_at", "?".bind(&dt.to_rfc3339()));
    }

    if let Some(sk) = &lq.is_skipped {
        sqlb.and_where_eq("is_skipped", sk);
    }
    if let Some(fs) = &lq.is_flow_step {
        sqlb.and_where_eq("is_flow_step", fs);
    }
    if let Some(fs) = &lq.has_null_parent {
        if *fs {
            sqlb.and_where_is_null("parent_job");
        }
    }
    if let Some(jk) = &lq.job_kinds {
        sqlb.and_where_in(
            "job_kind",
            &jk.split(',').into_iter().map(quote).collect::<Vec<_>>(),
        );
    }

    if let Some(args) = &lq.args {
        sqlb.and_where("args @> ?".bind(&args.replace("'", "''")));
    }

    if let Some(result) = &lq.result {
        sqlb.and_where("result @> ?".bind(&result.replace("'", "''")));
    }

    if lq.is_not_schedule.unwrap_or(false) {
        sqlb.and_where("schedule_path IS null");
    }

    sqlb
}

pub fn list_completed_jobs_query(
    w_id: &str,
    per_page: usize,
    offset: usize,
    lq: &ListCompletedQuery,
    fields: &[&str],
    join_outstanding_wait_times: bool,
    tags: Option<Vec<&str>>,
) -> SqlBuilder {
    let mut sqlb = SqlBuilder::select_from("completed_job")
        .fields(fields)
        .order_by("created_at", lq.order_desc.unwrap_or(true))
        .offset(offset)
        .limit(per_page)
        .clone();

    if let Some(tags) = tags {
        sqlb.and_where_in("tag", &tags.iter().map(|x| quote(x)).collect::<Vec<_>>());
    }

    filter_list_completed_query(sqlb, lq, w_id, join_outstanding_wait_times)
}
#[derive(Deserialize, Clone)]
pub struct ListCompletedQuery {
    pub script_path_start: Option<String>,
    pub script_path_exact: Option<String>,
    pub script_hash: Option<String>,
    pub created_by: Option<String>,
    pub started_before: Option<chrono::DateTime<chrono::Utc>>,
    pub started_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_before: Option<chrono::DateTime<chrono::Utc>>,
    pub created_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_or_started_before: Option<chrono::DateTime<chrono::Utc>>,
    pub created_or_started_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_or_started_after_completed_jobs: Option<chrono::DateTime<chrono::Utc>>,
    pub success: Option<bool>,
    pub running: Option<bool>,
    pub parent_job: Option<String>,
    pub order_desc: Option<bool>,
    pub job_kinds: Option<String>,
    pub is_skipped: Option<bool>,
    pub is_flow_step: Option<bool>,
    pub suspended: Option<bool>,
    pub schedule_path: Option<String>,
    // filter by matching a subset of the args using base64 encoded json subset
    pub args: Option<String>,
    // filter by matching a subset of the result using base64 encoded json subset
    pub result: Option<String>,
    pub tag: Option<String>,
    pub scheduled_for_before_now: Option<bool>,
    pub all_workspaces: Option<bool>,
    pub has_null_parent: Option<bool>,
    pub label: Option<String>,
    pub is_not_schedule: Option<bool>,
    pub concurrency_key: Option<String>,
}

async fn list_completed_jobs(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(pagination): Query<Pagination>,
    Query(lq): Query<ListCompletedQuery>,
) -> error::JsonResult<Vec<ListableCompletedJob>> {
    check_scopes(&authed, || format!("jobs:listjobs"))?;

    let (per_page, offset) = paginate(pagination);

    let sql = list_completed_jobs_query(
        &w_id,
        per_page,
        offset,
        &lq,
        &[
            "id",
            "workspace_id",
            "parent_job",
            "created_by",
            "created_at",
            "started_at",
            "duration_ms",
            "success",
            "script_hash",
            "script_path",
            "deleted",
            "canceled",
            "canceled_by",
            "canceled_reason",
            "job_kind",
            "schedule_path",
            "permissioned_as",
            "null as raw_code",
            "null as flow_status",
            "null as raw_flow",
            "is_flow_step",
            "language",
            "is_skipped",
            "email",
            "visible_to_owner",
            "mem_peak",
            "tag",
            "priority",
            "result->'wm_labels' as labels",
            "'CompletedJob' as type",
        ],
        false,
        get_scope_tags(&authed),
    )
    .sql()?;
    let mut tx = user_db.begin(&authed).await?;
    let jobs = sqlx::query_as::<_, ListableCompletedJob>(&sql)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(jobs))
}

async fn get_completed_job<'a>(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
) -> error::Result<Response> {
    let tags = opt_authed
        .as_ref()
        .map(|authed| get_scope_tags(authed))
        .flatten();

    let job_o = GetQuery::new()
        .with_auth(&opt_authed)
        .with_in_tags(tags.as_ref())
        .fetch_completed(&db, id, &w_id)
        .await?;

    let cj = not_found_if_none(job_o, "Completed Job", id.to_string())?;
    let response = Json(cj).into_response();
    // let extra_log = query_scalar!(
    //     "SELECT substr(logs, $1) as logs FROM large_logs WHERE workspace_id = $2 AND job_id = $3",
    //     log_offset - len,
    //     &w_id,
    //     &job_id
    // )
    // .fetch_optional(db)
    // .await.ok().flatten().flatten();

    log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

    Ok(response)
}

#[derive(FromRow)]
pub struct RawResult {
    pub result: Option<sqlx::types::Json<Box<RawValue>>>,
    pub flow_status: Option<sqlx::types::Json<Box<RawValue>>>,
    pub language: Option<ScriptLang>,
    pub created_by: Option<String>,
}

#[derive(FromRow)]
pub struct RawResultWithSuccess {
    pub result: Option<sqlx::types::Json<Box<RawValue>>>,
    pub flow_status: Option<sqlx::types::Json<Box<RawValue>>>,
    pub language: Option<ScriptLang>,
    pub success: bool,
    pub created_by: String,
}

async fn get_completed_job_result(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
    Query(JsonPath { json_path, suspended_job, approver, resume_id, secret }): Query<JsonPath>,
) -> error::Result<Response> {
    let tags = opt_authed
        .as_ref()
        .map(|authed| get_scope_tags(authed))
        .flatten();
    let result_o = if let Some(json_path) = json_path {
        sqlx::query_as::<_, RawResult>(
            "SELECT result #> $3 as result, flow_status, language, created_by FROM completed_job WHERE id = $1 AND workspace_id = $2 AND ($4::text[] IS NULL OR tag = ANY($4))",
        )
        .bind(id)
        .bind(&w_id)
        .bind(
            json_path
                .split(".")
                .map(|x| x.to_string())
                .collect::<Vec<_>>(),
        )
        .bind(tags.as_ref().map(|v| v.as_slice()))
        .fetch_optional(&db)
        .await?
    } else {
        sqlx::query_as::<_, RawResult>("SELECT result, flow_status, language, created_by FROM completed_job WHERE id = $1 AND workspace_id = $2 AND ($3::text[] IS NULL OR tag = ANY($3))")
            .bind(id)
            .bind(&w_id)
            .bind(tags.as_ref().map(|v| v.as_slice()))
            .fetch_optional(&db)
            .await?
    };

    let mut raw_result = not_found_if_none(result_o, "Completed Job", id.to_string())?;

    if opt_authed.is_none() && raw_result.created_by.unwrap_or_default() != "anonymous" {
        match (suspended_job, resume_id, approver, secret) {
            (Some(suspended_job), Some(resume_id), approver, Some(secret)) => {
                let mut parent_job = id;
                while parent_job != suspended_job {
                    let p_job = sqlx::query_scalar!(
                        "SELECT parent_job FROM queue WHERE id = $1 AND workspace_id = $2",
                        parent_job,
                        &w_id
                    )
                    .fetch_optional(&db)
                    .await?
                    .flatten();
                    if let Some(p_job) = p_job {
                        parent_job = p_job;
                    } else {
                        return Err(Error::BadRequest("Approval secret of suspended job is not a parent of the job whose id's is being searched not found".to_string()));
                    }
                }
                verify_suspended_secret(
                    &w_id,
                    &db,
                    suspended_job,
                    resume_id,
                    &QueryApprover { approver },
                    secret,
                )
                .await?
            }
            _ => {
                return Err(Error::BadRequest(
                    "As a non logged in user, you can only see jobs ran by anonymous users"
                        .to_string(),
                ))
            }
        }
    }

    format_result(
        raw_result.language.as_ref(),
        raw_result.flow_status.as_ref(),
        raw_result.result.as_mut(),
    );

    log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

    Ok(Json(raw_result.result).into_response())
}

#[derive(Deserialize)]
struct CountByTagQuery {
    horizon_secs: Option<i64>,
    workspace_id: Option<String>,
}

#[derive(Serialize)]
struct TagCount {
    tag: String,
    count: i64,
}

async fn count_by_tag(
    ApiAuthed { email, .. }: ApiAuthed,
    Extension(db): Extension<DB>,
    Query(query): Query<CountByTagQuery>,
) -> JsonResult<Vec<TagCount>> {
    require_super_admin(&db, &email).await?;
    let horizon = query.horizon_secs.unwrap_or(3600); // Default to 1 hour if not specified

    let counts = sqlx::query_as!(
        TagCount,
        r#"
        SELECT tag as "tag!", COUNT(*) as "count!"
        FROM completed_job
        WHERE started_at > NOW() - make_interval(secs => $1) AND ($2::text IS NULL OR workspace_id = $2)
        GROUP BY tag
        ORDER BY "count!" DESC
        "#,
        horizon as f64,
        query.workspace_id
    )
    .fetch_all(&db)
    .await?;

    Ok(Json(counts))
}

#[derive(Serialize)]
struct CompletedJobResult {
    started: Option<bool>,
    success: Option<bool>,
    completed: bool,
    result: Option<sqlx::types::Json<Box<RawValue>>>,
}

#[derive(Deserialize)]
struct GetCompletedJobQuery {
    get_started: Option<bool>,
}

async fn get_completed_job_result_maybe(
    OptAuthed(opt_authed): OptAuthed,
    Extension(db): Extension<DB>,
    Path((w_id, id)): Path<(String, Uuid)>,
    Query(GetCompletedJobQuery { get_started }): Query<GetCompletedJobQuery>,
) -> error::Result<Response> {
    let tags = opt_authed
        .as_ref()
        .map(|authed| get_scope_tags(authed))
        .flatten();
    let result_o = sqlx::query_as::<_, RawResultWithSuccess>(
        "SELECT result, success, language, flow_status, created_by FROM completed_job WHERE id = $1 AND workspace_id = $2 AND ($3::text[] IS NULL OR tag = ANY($3))",
    )
    .bind(id)
    .bind(&w_id)
    .bind(tags.as_ref().map(|v| v.as_slice()))
    .fetch_optional(&db)
    .await?;

    if let Some(mut res) = result_o {
        format_result(
            res.language.as_ref(),
            res.flow_status.as_ref(),
            res.result.as_mut(),
        );
        if opt_authed.is_none() && res.created_by != "anonymous" {
            return Err(Error::BadRequest(
                "As a non logged in user, you can only see jobs ran by anonymous users".to_string(),
            ));
        }

        log_job_view(&db, opt_authed.as_ref(), &w_id, &id).await?;

        Ok(Json(CompletedJobResult {
            started: Some(true),
            success: Some(res.success),
            completed: true,
            result: res.result,
        })
        .into_response())
    } else if get_started.is_some_and(|x| x) {
        let started = sqlx::query_scalar!(
            "SELECT running FROM queue WHERE id = $1 AND workspace_id = $2",
            id,
            w_id
        )
        .fetch_optional(&db)
        .await?
        .unwrap_or(false);
        Ok(Json(CompletedJobResult {
            started: Some(started),
            completed: false,
            success: None,
            result: None,
        })
        .into_response())
    } else {
        Ok(Json(CompletedJobResult {
            started: None,
            completed: false,
            success: None,
            result: None,
        })
        .into_response())
    }
}

async fn delete_completed_job<'a>(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, id)): Path<(String, Uuid)>,
) -> error::Result<Response> {
    check_scopes(&authed, || format!("jobs:deletejob"))?;

    let mut tx = user_db.begin(&authed).await?;

    require_admin(authed.is_admin, &authed.username)?;
    let tags = get_scope_tags(&authed);
    let job_o = sqlx::query_as::<_, CompletedJob>(
        "UPDATE completed_job SET args = null, logs = '', result = null, deleted = true WHERE id = $1 AND workspace_id = $2 AND ($3::text[] IS NULL OR tag = ANY($3)) \
         RETURNING *, null as labels",
    )
    .bind(id)
    .bind(&w_id)
    .bind(tags.as_ref().map(|v| v.as_slice()))
    .fetch_optional(&mut *tx)
    .await?;

    let cj = not_found_if_none(job_o, "Completed Job", id.to_string())?;

    sqlx::query!("DELETE FROM job_logs WHERE job_id = $1", id)
        .execute(&mut *tx)
        .await?;

    audit_log(
        &mut *tx,
        &authed,
        "jobs.delete",
        ActionKind::Delete,
        &w_id,
        Some(&id.to_string()),
        None,
    )
    .await?;

    tx.commit().await?;

    let cj = format_completed_job_result(cj);

    let response = Json(cj).into_response();
    Ok(response)
}
