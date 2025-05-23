#[cfg(feature = "parquet")]
use crate::job_helpers_ee::get_workspace_s3_resource;
use crate::{
    args::WebhookArgs,
    auth::{AuthCache, OptTokened},
    db::{ApiAuthed, DB},
    jobs::{
        run_flow_by_path_inner, run_script_by_path_inner, run_wait_result_flow_by_path_internal,
        run_wait_result_script_by_path_internal, RunJobQuery,
    },
    users::fetch_api_authed,
};
use axum::{
    extract::{Path, Query},
    response::IntoResponse,
    routing::{delete, get, post},
    Extension, Json, Router,
};
#[cfg(feature = "parquet")]
use http::header::IF_NONE_MATCH;
use http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use sql_builder::{bind::Bind, SqlBuilder};
use sqlx::prelude::FromRow;
use std::{collections::HashMap, sync::Arc};
use tower_http::cors::CorsLayer;
use windmill_audit::{audit_ee::audit_log, ActionKind};
#[cfg(feature = "parquet")]
use windmill_common::s3_helpers::build_object_store_client;
use windmill_common::{
    db::UserDB,
    error::{self, JsonResult},
    s3_helpers::S3Object,
    utils::{not_found_if_none, paginate, require_admin, Pagination, StripPath},
    worker::{to_raw_value, CLOUD_HOSTED},
};

lazy_static::lazy_static! {
    static ref ROUTE_PATH_KEY_RE: regex::Regex = regex::Regex::new(r"/:\w+").unwrap();
}

pub fn routes_global_service() -> Router {
    let cors = CorsLayer::new()
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::DELETE,
            http::Method::PUT,
            http::Method::PATCH,
        ])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
        .allow_origin(tower_http::cors::Any);
    Router::new()
        .route(
            "/*path",
            get(route_job)
                .post(route_job)
                .delete(route_job)
                .put(route_job)
                .patch(route_job)
                .head(|| async { "" }),
        )
        .layer(cors)
}

pub fn workspaced_service() -> Router {
    Router::new()
        .route("/create", post(create_trigger))
        .route("/list", get(list_triggers))
        .route("/get/*path", get(get_trigger))
        .route("/update/*path", post(update_trigger))
        .route("/delete/*path", delete(delete_trigger))
        .route("/exists/*path", get(exists_trigger))
        .route("/route_exists", post(exists_route))
}

#[derive(Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "HTTP_METHOD", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl TryFrom<&http::Method> for HttpMethod {
    type Error = error::Error;
    fn try_from(method: &http::Method) -> Result<Self, Self::Error> {
        match method {
            &http::Method::GET => Ok(HttpMethod::Get),
            &http::Method::POST => Ok(HttpMethod::Post),
            &http::Method::PUT => Ok(HttpMethod::Put),
            &http::Method::DELETE => Ok(HttpMethod::Delete),
            &http::Method::PATCH => Ok(HttpMethod::Patch),
            _ => Err(error::Error::BadRequest("Invalid HTTP method".to_string())),
        }
    }
}

#[derive(Deserialize)]
struct NewTrigger {
    path: String,
    route_path: String,
    script_path: String,
    is_flow: bool,
    is_async: bool,
    requires_auth: bool,
    http_method: HttpMethod,
    static_asset_config: Option<sqlx::types::Json<S3Object>>,
}

#[derive(FromRow, Serialize)]
struct Trigger {
    workspace_id: String,
    path: String,
    route_path: String,
    route_path_key: String,
    script_path: String,
    is_flow: bool,
    edited_by: String,
    email: String,
    edited_at: chrono::DateTime<chrono::Utc>,
    extra_perms: serde_json::Value,
    is_async: bool,
    requires_auth: bool,
    http_method: HttpMethod,
    static_asset_config: Option<sqlx::types::Json<S3Object>>,
}

#[derive(Deserialize)]
struct EditTrigger {
    path: String,
    route_path: Option<String>,
    script_path: String,
    is_flow: bool,
    is_async: bool,
    requires_auth: bool,
    http_method: HttpMethod,
    static_asset_config: Option<sqlx::types::Json<S3Object>>,
}

#[derive(Deserialize)]
pub struct ListTriggerQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub path: Option<String>,
    pub is_flow: Option<bool>,
    pub path_start: Option<String>,
}

async fn list_triggers(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Query(lst): Query<ListTriggerQuery>,
) -> error::JsonResult<Vec<Trigger>> {
    let mut tx = user_db.begin(&authed).await?;
    let (per_page, offset) = paginate(Pagination { per_page: lst.per_page, page: lst.page });
    let mut sqlb = SqlBuilder::select_from("http_trigger")
        .field("*")
        .order_by("edited_at", true)
        .and_where("workspace_id = ?".bind(&w_id))
        .offset(offset)
        .limit(per_page)
        .clone();
    if let Some(path) = lst.path {
        sqlb.and_where_eq("script_path", "?".bind(&path));
    }
    if let Some(is_flow) = lst.is_flow {
        sqlb.and_where_eq("is_flow", "?".bind(&is_flow));
    }
    if let Some(path_start) = &lst.path_start {
        sqlb.and_where_like_left("path", path_start);
    }
    let sql = sqlb
        .sql()
        .map_err(|e| error::Error::InternalErr(e.to_string()))?;
    let rows = sqlx::query_as::<_, Trigger>(&sql)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Json(rows))
}

async fn get_trigger(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, path)): Path<(String, StripPath)>,
) -> error::JsonResult<Trigger> {
    let mut tx = user_db.begin(&authed).await?;
    let path = path.to_path();
    let trigger = sqlx::query_as!(
        Trigger,
        r#"SELECT workspace_id, path, route_path, route_path_key, script_path, is_flow, http_method as "http_method: _", edited_by, email, edited_at, extra_perms, is_async, requires_auth, static_asset_config as "static_asset_config: _"
            FROM http_trigger
            WHERE workspace_id = $1 AND path = $2"#,
        w_id,
        path,
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let trigger = not_found_if_none(trigger, "Trigger", path)?;

    Ok(Json(trigger))
}

async fn create_trigger(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path(w_id): Path<String>,
    Json(ct): Json<NewTrigger>,
) -> error::Result<(StatusCode, String)> {
    require_admin(authed.is_admin, &authed.username)?;

    let route_path_key = ROUTE_PATH_KEY_RE.replace_all(ct.route_path.as_str(), ":key");

    let mut tx = user_db.begin(&authed).await?;
    sqlx::query!(
        "INSERT INTO http_trigger (workspace_id, path, route_path, route_path_key, script_path, is_flow, is_async, requires_auth, http_method, static_asset_config, edited_by, email, edited_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now())",
        w_id,
        ct.path,
        ct.route_path,
        &route_path_key,
        ct.script_path,
        ct.is_flow,
        ct.is_async,
        ct.requires_auth,
        ct.http_method as _,
        ct.static_asset_config as _,
        &authed.username,
        &authed.email
    )
    .execute(&mut *tx).await?;

    audit_log(
        &mut *tx,
        &authed,
        "http_triggers.create",
        ActionKind::Create,
        &w_id,
        Some(ct.path.as_str()),
        None,
    )
    .await?;

    tx.commit().await?;

    Ok((StatusCode::CREATED, format!("{}", ct.path)))
}

async fn update_trigger(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, path)): Path<(String, StripPath)>,
    Json(ct): Json<EditTrigger>,
) -> error::Result<String> {
    let path = path.to_path();
    let mut tx = user_db.begin(&authed).await?;

    if authed.is_admin {
        if ct.route_path.is_none() {
            return Err(error::Error::BadRequest(
                "route_path is required".to_string(),
            ));
        }

        let route_path_key =
            ROUTE_PATH_KEY_RE.replace_all(ct.route_path.as_ref().unwrap().as_str(), ":key");

        sqlx::query!(
            "UPDATE http_trigger 
                SET route_path = $1, route_path_key = $2, script_path = $3, path = $4, is_flow = $5, http_method = $6, static_asset_config = $7, edited_by = $8, email = $9, is_async = $10, requires_auth = $11, edited_at = now() 
                WHERE workspace_id = $12 AND path = $13",
            ct.route_path,
            &route_path_key,
            ct.script_path,
            ct.path,
            ct.is_flow,
            ct.http_method as _,
            ct.static_asset_config as _,
            &authed.username,
            &authed.email,
            ct.is_async,
            ct.requires_auth,
            w_id,
            path,
        )
        .execute(&mut *tx).await?;
    } else {
        sqlx::query!(
            "UPDATE http_trigger SET script_path = $1, path = $2, is_flow = $3, http_method = $4, static_asset_config = $5, edited_by = $6, email = $7, is_async = $8, requires_auth = $9, edited_at = now() 
                WHERE workspace_id = $10 AND path = $11",
            ct.script_path,
            ct.path,
            ct.is_flow,
            ct.http_method as _,
            ct.static_asset_config as _,
            &authed.username,
            &authed.email,
            ct.is_async,
            ct.requires_auth,
            w_id,
            path,
        )
        .execute(&mut *tx).await?;
    }

    audit_log(
        &mut *tx,
        &authed,
        "http_triggers.update",
        ActionKind::Create,
        &w_id,
        Some(path),
        None,
    )
    .await?;

    tx.commit().await?;

    Ok(path.to_string())
}

async fn delete_trigger(
    authed: ApiAuthed,
    Extension(user_db): Extension<UserDB>,
    Path((w_id, path)): Path<(String, StripPath)>,
) -> error::Result<String> {
    require_admin(authed.is_admin, &authed.username)?;
    let path = path.to_path();
    let mut tx = user_db.begin(&authed).await?;
    sqlx::query!(
        "DELETE FROM http_trigger WHERE workspace_id = $1 AND path = $2",
        w_id,
        path,
    )
    .execute(&mut *tx)
    .await?;

    audit_log(
        &mut *tx,
        &authed,
        "http_triggers.delete",
        ActionKind::Delete,
        &w_id,
        Some(path),
        None,
    )
    .await?;

    tx.commit().await?;

    Ok(format!("HTTP trigger {path} deleted"))
}

async fn exists_trigger(
    Extension(db): Extension<DB>,
    Path((w_id, path)): Path<(String, StripPath)>,
) -> JsonResult<bool> {
    let path = path.to_path();
    let exists = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM http_trigger WHERE path = $1 AND workspace_id = $2)",
        path,
        w_id,
    )
    .fetch_one(&db)
    .await?
    .unwrap_or(false);
    Ok(Json(exists))
}

#[derive(Deserialize)]
struct RouteExists {
    route_path: String,
    http_method: HttpMethod,
}

async fn exists_route(
    Extension(db): Extension<DB>,
    Path(w_id): Path<String>,
    Json(RouteExists { route_path, http_method }): Json<RouteExists>,
) -> JsonResult<bool> {
    let route_path_key = ROUTE_PATH_KEY_RE.replace_all(route_path.as_str(), ":key");
    let exists = if *CLOUD_HOSTED {
        sqlx::query_scalar!(
                    "SELECT EXISTS(SELECT 1 FROM http_trigger WHERE route_path_key = $1 AND workspace_id = $2 AND http_method = $3)",
                    &route_path_key,
                    w_id,
                    http_method as HttpMethod
                )
                .fetch_one(&db)
                .await?
                .unwrap_or(false)
    } else {
        sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM http_trigger WHERE route_path_key = $1 AND http_method = $2)",
            &route_path_key,
            http_method as HttpMethod
        )
        .fetch_one(&db)
        .await?
        .unwrap_or(false)
    };
    Ok(Json(exists))
}

struct TriggerRoute {
    path: String,
    script_path: String,
    is_flow: bool,
    route_path: String,
    workspace_id: String,
    is_async: bool,
    requires_auth: bool,
    edited_by: String,
    email: String,
    static_asset_config: Option<sqlx::types::Json<S3Object>>,
}

async fn get_http_route_trigger(
    route_path: &str,
    auth_cache: &Arc<AuthCache>,
    token: Option<&String>,
    db: &DB,
    user_db: UserDB,
    method: &http::Method,
) -> error::Result<(TriggerRoute, String, HashMap<String, String>, ApiAuthed)> {
    let http_method: HttpMethod = method.try_into()?;
    let (mut triggers, route_path) = if *CLOUD_HOSTED {
        let mut splitted = route_path.split("/");
        let w_id = splitted.next().ok_or_else(|| {
            error::Error::BadRequest("Missing workspace id in route path".to_string())
        })?;
        let route_path = StripPath(splitted.collect::<Vec<_>>().join("/"));
        let triggers = sqlx::query_as!(
            TriggerRoute,
            r#"SELECT path, script_path, is_flow, route_path, workspace_id, is_async, requires_auth, edited_by, email, static_asset_config as "static_asset_config: _" FROM http_trigger WHERE workspace_id = $1 AND http_method = $2"#,
            w_id,
            http_method as HttpMethod
        )
        .fetch_all(db)
        .await?;
        (triggers, route_path)
    } else {
        let triggers = sqlx::query_as!(
            TriggerRoute,
            r#"SELECT path, script_path, is_flow, route_path, workspace_id, is_async, requires_auth, edited_by, email, static_asset_config as "static_asset_config: _" FROM http_trigger WHERE http_method = $1"#,
            http_method as HttpMethod
        )
        .fetch_all(db)
        .await?;
        (triggers, StripPath(route_path.to_string()))
    };

    let mut router = matchit::Router::new();

    for (idx, trigger) in triggers.iter().enumerate() {
        let route_path = trigger.route_path.clone();
        router.insert(route_path.as_str(), idx).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to consider http trigger route {}: {:?}",
                route_path,
                e,
            );
        });
    }

    let trigger_idx = router.at(route_path.0.as_str()).ok();

    let matchit::Match { value: trigger_idx, params } =
        not_found_if_none(trigger_idx, "Trigger", route_path.0.as_str())?;

    let trigger = triggers.remove(trigger_idx.to_owned());

    let params: HashMap<String, String> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let username_override = if trigger.requires_auth {
        let opt_authed = if let Some(token) = token {
            auth_cache
                .get_authed(Some(trigger.workspace_id.clone()), token)
                .await
        } else {
            None
        };
        if let Some(authed) = opt_authed {
            // check that the user has access to the trigger
            let mut tx = user_db.begin(&authed).await?;
            let exists = sqlx::query_scalar!(
                "SELECT EXISTS(SELECT 1 FROM http_trigger WHERE workspace_id = $1 AND path = $2)",
                trigger.workspace_id,
                trigger.path
            )
            .fetch_one(&mut *tx)
            .await?
            .unwrap_or(false);
            tx.commit().await?;
            if exists {
                Some(authed.display_username().to_owned())
            } else {
                return Err(error::Error::NotAuthorized("Unauthorized".to_string()));
            }
        } else {
            return Err(error::Error::NotAuthorized(
                "Requires authentication".to_string(),
            ));
        }
    } else {
        None
    };

    let authed = fetch_api_authed(
        trigger.edited_by.clone(),
        trigger.email.clone(),
        &trigger.workspace_id,
        &db,
        Some(username_override.unwrap_or(format!("http-{}", trigger.path))),
    )
    .await?;

    Ok((trigger, route_path.0, params, authed))
}

pub async fn build_http_trigger_extra(
    route_path: &str,
    called_path: &str,
    method: &http::Method,
    params: &HashMap<String, String>,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Box<serde_json::value::RawValue> {
    let headers = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect::<HashMap<String, String>>();

    to_raw_value(&serde_json::json!({
        "kind": "http",
        "http": {
            "route": route_path,
            "path": called_path,
            "method": method.to_string().to_lowercase(),
            "params": params,
            "query": query,
            "headers": headers
        },
    }))
}

async fn route_job(
    Extension(db): Extension<DB>,
    Extension(user_db): Extension<UserDB>,
    Extension(auth_cache): Extension<Arc<AuthCache>>,
    OptTokened { token }: OptTokened,
    Path(route_path): Path<StripPath>,
    Query(query): Query<HashMap<String, String>>,
    method: http::Method,
    headers: HeaderMap,
    args: WebhookArgs,
) -> impl IntoResponse {
    let route_path = route_path.to_path();
    let (trigger, called_path, params, authed) = match get_http_route_trigger(
        route_path,
        &auth_cache,
        token.as_ref(),
        &db,
        user_db.clone(),
        &method,
    )
    .await
    {
        Ok(trigger) => trigger,
        Err(e) => return e.into_response(),
    };

    let mut args = match args
        .to_push_args_owned(&authed, &db, &trigger.workspace_id)
        .await
    {
        Ok(args) => args,
        Err(e) => return e.into_response(),
    };

    #[cfg(not(feature = "parquet"))]
    if trigger.static_asset_config.is_some() {
        return error::Error::InternalErr(
            "Static asset configuration is not supported in this build".to_string(),
        )
        .into_response();
    }

    #[cfg(feature = "parquet")]
    if let Some(sqlx::types::Json(config)) = trigger.static_asset_config {
        let build_static_response_f = async {
            let (_, s3_resource_opt) = get_workspace_s3_resource(
                &authed,
                &db,
                None,
                &"NO_TOKEN".to_string(), // no token is provided in this case
                &trigger.workspace_id,
                config.storage,
            )
            .await?;
            let s3_resource = s3_resource_opt.ok_or(error::Error::InternalErr(
                "No files storage resource defined at the workspace level".to_string(),
            ))?;
            let s3_client = build_object_store_client(&s3_resource).await?;
            let path = object_store::path::Path::from(config.s3);
            let s3_object = s3_client.get(&path).await.map_err(|err| {
                tracing::warn!("Error retrieving file from S3: {:?}", err);
                error::Error::InternalErr(format!("Error retrieving file: {}", err.to_string()))
            })?;
            let mut response_headers = http::HeaderMap::new();
            if let Some(ref e_tag) = s3_object.meta.e_tag {
                if let Some(if_none_match) = headers.get(IF_NONE_MATCH) {
                    if if_none_match == e_tag {
                        return Ok::<_, error::Error>((
                            StatusCode::NOT_MODIFIED,
                            response_headers,
                            axum::body::Body::empty(),
                        ));
                    }
                }
                if let Ok(e_tag) = e_tag.parse() {
                    response_headers.insert("etag", e_tag);
                }
            }
            response_headers.insert(
                "content-type",
                s3_object
                    .attributes
                    .get(&object_store::Attribute::ContentType)
                    .map(|s| s.parse().ok())
                    .flatten()
                    .unwrap_or("application/octet-stream".parse().unwrap()),
            );
            response_headers.insert(
                "content-disposition",
                config.filename.as_ref().map_or_else(
                    || {
                        s3_object
                            .attributes
                            .get(&object_store::Attribute::ContentDisposition)
                            .map(|s| s.parse().ok())
                            .flatten()
                            .unwrap_or("inline".parse().unwrap())
                    },
                    |filename| {
                        format!("inline; filename=\"{}\"", filename)
                            .parse()
                            .unwrap_or("inline".parse().unwrap())
                    },
                ),
            );

            let body_stream = axum::body::Body::from_stream(s3_object.into_stream());
            Ok::<_, error::Error>((StatusCode::OK, response_headers, body_stream))
        };
        match build_static_response_f.await {
            Ok((status, headers, body_stream)) => {
                return (status, headers, body_stream).into_response()
            }
            Err(e) => return e.into_response(),
        }
    }

    let extra = args.extra.get_or_insert_with(HashMap::new);
    extra.insert(
        "wm_trigger".to_string(),
        build_http_trigger_extra(
            &trigger.route_path,
            &called_path,
            &method,
            &params,
            &query,
            &headers,
        )
        .await,
    );

    let run_query = RunJobQuery::default();

    if trigger.is_flow {
        if trigger.is_async {
            run_flow_by_path_inner(
                authed,
                db,
                user_db,
                trigger.workspace_id.clone(),
                StripPath(trigger.script_path.to_owned()),
                run_query,
                args,
                None,
            )
            .await
            .into_response()
        } else {
            run_wait_result_flow_by_path_internal(
                db,
                run_query,
                StripPath(trigger.script_path.to_owned()),
                authed,
                user_db,
                args,
                trigger.workspace_id.clone(),
                None,
            )
            .await
            .into_response()
        }
    } else {
        if trigger.is_async {
            run_script_by_path_inner(
                authed,
                db,
                user_db,
                trigger.workspace_id.clone(),
                StripPath(trigger.script_path.to_owned()),
                run_query,
                args,
                None,
            )
            .await
            .into_response()
        } else {
            run_wait_result_script_by_path_internal(
                db,
                run_query,
                StripPath(trigger.script_path.to_owned()),
                authed,
                user_db,
                trigger.workspace_id.clone(),
                args,
                None,
            )
            .await
            .into_response()
        }
    }
}
