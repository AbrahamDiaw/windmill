/*
 * Author: Ruben Fiszel
 * Copyright: Windmill Labs, Inc 2022
 * This file and its contents are licensed under the AGPLv3 License.
 * Please see the included NOTICE for copyright information and
 * LICENSE-AGPL for a copy of the license.
 */

use crate::db::ApiAuthed;
#[cfg(feature = "enterprise")]
use crate::ee::ExternalJwks;
#[cfg(feature = "embedding")]
use crate::embeddings::load_embeddings_db;
#[cfg(feature = "oauth2")]
use crate::oauth2_ee::AllClients;
#[cfg(feature = "oauth2")]
use crate::oauth2_ee::SlackVerifier;
#[cfg(feature = "smtp")]
use crate::smtp_server_ee::SmtpServer;

use crate::tracing_init::MyOnFailure;
use crate::{
    tracing_init::{MyMakeSpan, MyOnResponse},
    users::OptAuthed,
    webhook_util::WebhookShared,
};

use anyhow::Context;
use argon2::Argon2;
use axum::extract::DefaultBodyLimit;
use axum::{middleware::from_extractor, routing::get, routing::post, Extension, Router};
use db::DB;
use http::HeaderValue;
use reqwest::Client;
#[cfg(feature = "oauth2")]
use std::collections::HashMap;

use std::time::Duration;
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use tower::ServiceBuilder;
use tower_cookies::CookieManagerLayer;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use windmill_common::db::UserDB;
use windmill_common::worker::CLOUD_HOSTED;
use windmill_common::{utils::GIT_VERSION, BASE_URL, INSTANCE_NAME};

use crate::scim_ee::has_scim_token;
use windmill_common::error::AppError;

mod ai;
mod apps;
mod args;
mod audit;
mod auth;
mod capture;
mod concurrency_groups;
mod configs;
mod db;
mod drafts;
pub mod ee;
pub mod embeddings;
mod favorite;
mod flows;
mod folders;
mod granular_acls;
mod groups;
#[cfg(feature = "http_trigger")]
mod http_triggers;
mod indexer_ee;
mod inputs;
mod integration;
#[cfg(feature = "postgres_trigger")]
mod postgres_triggers;

#[cfg(feature = "enterprise")]
mod apps_ee;
#[cfg(feature = "parquet")]
mod job_helpers_ee;
pub mod job_metrics;
pub mod jobs;
#[cfg(all(feature = "enterprise", feature = "kafka"))]
mod kafka_triggers_ee;
#[cfg(all(feature = "enterprise", feature = "nats"))]
mod nats_triggers_ee;
#[cfg(feature = "oauth2")]
pub mod oauth2_ee;
mod oidc_ee;
mod raw_apps;
mod resources;
mod saml_ee;
mod schedule;
mod scim_ee;
mod scripts;
mod service_logs;
mod settings;
mod slack_approvals;
#[cfg(feature = "smtp")]
mod smtp_server_ee;
mod static_assets;
mod stripe_ee;
#[cfg(feature = "enterprise")]
mod teams_ee;
mod tracing_init;
mod triggers;
mod users;
mod users_ee;
mod utils;
mod variables;
mod webhook_util;
#[cfg(feature = "websocket")]
mod websocket_triggers;
mod workers;
mod workspaces;
mod workspaces_ee;
mod workspaces_export;
mod workspaces_extra;

pub const DEFAULT_BODY_LIMIT: usize = 2097152 * 100; // 200MB

lazy_static::lazy_static! {

    pub static ref REQUEST_SIZE_LIMIT: Arc<RwLock<usize>> = Arc::new(RwLock::new(DEFAULT_BODY_LIMIT));

    pub static ref SCIM_TOKEN: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    pub static ref SAML_METADATA: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));


    pub static ref COOKIE_DOMAIN: Option<String> = std::env::var("COOKIE_DOMAIN").ok();

    pub static ref IS_SECURE: Arc<RwLock<bool>> = Arc::new(RwLock::new(false));

    pub static ref HTTP_CLIENT: Client = reqwest::ClientBuilder::new()
        .user_agent("windmill/beta")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(std::env::var("ACCEPT_INVALID_CERTS").is_ok())
        .build().unwrap();


}

#[cfg(feature = "oauth2")]
lazy_static::lazy_static! {
    pub static ref OAUTH_CLIENTS: Arc<RwLock<AllClients>> = Arc::new(RwLock::new(AllClients {
        logins: HashMap::new(),
        connects: HashMap::new(),
        slack: None
    }));


    pub static ref SLACK_SIGNING_SECRET: Option<SlackVerifier> = std::env::var("SLACK_SIGNING_SECRET")
        .ok()
        .map(|x| SlackVerifier::new(x).unwrap());

}

// Compliance with cloud events spec.
pub async fn add_webhook_allowed_origin(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.method() == http::Method::OPTIONS {
        if let Some(webhook_request_origin) = req.headers().get("Webhook-Request-Origin") {
            let webhook_request_origin = webhook_request_origin.clone();
            let mut response = next.run(req).await;

            response
                .headers_mut()
                .insert("Webhook-Allowed-Origin", webhook_request_origin);
            response
                .headers_mut()
                .insert("Webhook-Allowed-Rate", HeaderValue::from_static("*"));
            return response;
        }
    }
    next.run(req).await
}

#[cfg(not(feature = "tantivy"))]
type IndexReader = ();

#[cfg(not(feature = "tantivy"))]
type ServiceLogIndexReader = ();

#[cfg(feature = "tantivy")]
type IndexReader = windmill_indexer::completed_runs_ee::IndexReader;
#[cfg(feature = "tantivy")]
type ServiceLogIndexReader = windmill_indexer::service_logs_ee::ServiceLogIndexReader;

pub async fn run_server(
    db: DB,
    job_index_reader: Option<IndexReader>,
    log_index_reader: Option<ServiceLogIndexReader>,
    addr: SocketAddr,
    mut rx: tokio::sync::broadcast::Receiver<()>,
    port_tx: tokio::sync::oneshot::Sender<String>,
    server_mode: bool,
    #[cfg(feature = "smtp")] base_internal_url: String,
) -> anyhow::Result<()> {
    let user_db = UserDB::new(db.clone());

    #[cfg(feature = "enterprise")]
    let ext_jwks = ExternalJwks::load().await;
    let auth_cache = Arc::new(crate::auth::AuthCache::new(
        db.clone(),
        std::env::var("SUPERADMIN_SECRET").ok(),
        #[cfg(feature = "enterprise")]
        ext_jwks,
    ));
    let argon2 = Arc::new(Argon2::default());

    let disable_response_logs = std::env::var("DISABLE_RESPONSE_LOGS")
        .ok()
        .map(|x| x == "true")
        .unwrap_or(false);

    let middleware_stack = ServiceBuilder::new()
        .layer(Extension(db.clone()))
        .layer(Extension(user_db.clone()))
        .layer(Extension(auth_cache.clone()))
        .layer(Extension(job_index_reader))
        .layer(Extension(log_index_reader))
        // .layer(Extension(index_writer))
        .layer(CookieManagerLayer::new())
        .layer(Extension(WebhookShared::new(rx.resubscribe(), db.clone())))
        .layer(DefaultBodyLimit::max(
            REQUEST_SIZE_LIMIT.read().await.clone(),
        ));

    let cors = CorsLayer::new()
        .allow_methods([http::Method::GET, http::Method::POST])
        .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
        .allow_origin(Any);

    let sp_extension = Arc::new(saml_ee::build_sp_extension().await?);

    if server_mode {
        #[cfg(feature = "embedding")]
        load_embeddings_db(&db);

        #[cfg(feature = "smtp")]
        {
            let smtp_server = Arc::new(SmtpServer {
                db: db.clone(),
                user_db: user_db,
                auth_cache: auth_cache.clone(),
                base_internal_url: base_internal_url.clone(),
            });
            if let Err(err) = smtp_server.start_listener_thread(addr).await {
                tracing::error!("Error starting SMTP server: {err:#}");
            }
        }
    }

    let job_helpers_service = {
        #[cfg(feature = "parquet")]
        {
            job_helpers_ee::workspaced_service()
        }

        #[cfg(not(feature = "parquet"))]
        {
            Router::new()
        }
    };

    let kafka_triggers_service = {
        #[cfg(all(feature = "enterprise", feature = "kafka"))]
        {
            kafka_triggers_ee::workspaced_service()
        }

        #[cfg(not(all(feature = "enterprise", feature = "kafka")))]
        {
            Router::new()
        }
    };

    let nats_triggers_service = {
        #[cfg(all(feature = "enterprise", feature = "nats"))]
        {
            nats_triggers_ee::workspaced_service()
        }

        #[cfg(not(all(feature = "enterprise", feature = "nats")))]
        {
            Router::new()
        }
    };

    if !*CLOUD_HOSTED {
        #[cfg(feature = "websocket")]
        {
            let ws_killpill_rx = rx.resubscribe();
            websocket_triggers::start_websockets(db.clone(), ws_killpill_rx);
        }

        #[cfg(all(feature = "enterprise", feature = "kafka"))]
        {
            let kafka_killpill_rx = rx.resubscribe();
            kafka_triggers_ee::start_kafka_consumers(db.clone(), kafka_killpill_rx);
        }

        #[cfg(all(feature = "enterprise", feature = "nats"))]
        {
            let nats_killpill_rx = rx.resubscribe();
            nats_triggers_ee::start_nats_consumers(db.clone(), nats_killpill_rx);
        }
        #[cfg(feature = "postgres_trigger")]
        {
            let db_killpill_rx = rx.resubscribe();
            postgres_triggers::start_database(db.clone(), db_killpill_rx);
        }
    }

    // build our application with a route
    let app = Router::new()
        .nest(
            "/api",
            Router::new()
                .nest(
                    "/w/:workspace_id",
                    Router::new()
                        // Reordered alphabetically
                        .nest("/acls", granular_acls::workspaced_service())
                        .nest("/apps", apps::workspaced_service())
                        .nest("/audit", audit::workspaced_service())
                        .nest("/capture", capture::workspaced_service())
                        .nest(
                            "/concurrency_groups",
                            concurrency_groups::workspaced_service(),
                        )
                        .nest("/embeddings", embeddings::workspaced_service())
                        .nest("/drafts", drafts::workspaced_service())
                        .nest("/favorites", favorite::workspaced_service())
                        .nest("/flows", flows::workspaced_service())
                        .nest("/folders", folders::workspaced_service())
                        .nest("/groups", groups::workspaced_service())
                        .nest("/inputs", inputs::workspaced_service())
                        .nest("/job_metrics", job_metrics::workspaced_service())
                        .nest("/job_helpers", job_helpers_service)
                        .nest("/jobs", jobs::workspaced_service())
                        .nest("/oauth", {
                            #[cfg(feature = "oauth2")]
                            {
                                oauth2_ee::workspaced_service()
                            }

                            #[cfg(not(feature = "oauth2"))]
                            Router::new()
                        })
                        .nest("/ai", ai::workspaced_service())
                        .nest("/raw_apps", raw_apps::workspaced_service())
                        .nest("/resources", resources::workspaced_service())
                        .nest("/schedules", schedule::workspaced_service())
                        .nest("/scripts", scripts::workspaced_service())
                        .nest(
                            "/users",
                            users::workspaced_service().layer(Extension(argon2.clone())),
                        )
                        .nest("/variables", variables::workspaced_service())
                        .nest("/workspaces", workspaces::workspaced_service())
                        .nest("/oidc", oidc_ee::workspaced_service())
                        .nest("/http_triggers", {
                            #[cfg(feature = "http_trigger")]
                            {
                                http_triggers::workspaced_service()
                            }

                            #[cfg(not(feature = "http_trigger"))]
                            Router::new()
                        })
                        .nest("/websocket_triggers", {
                            #[cfg(feature = "websocket")]
                            {
                                websocket_triggers::workspaced_service()
                            }

                            #[cfg(not(feature = "websocket"))]
                            Router::new()
                        })
                        .nest("/kafka_triggers", kafka_triggers_service)
                        .nest("/nats_triggers", nats_triggers_service)
                        .nest("/postgres_triggers", {
                            #[cfg(feature = "postgres_trigger")]
                            {
                                postgres_triggers::workspaced_service()
                            }

                            #[cfg(not(feature = "postgres_trigger"))]
                            Router::new()
                        }),
                )
                .nest("/workspaces", workspaces::global_service())
                .nest(
                    "/users",
                    users::global_service().layer(Extension(argon2.clone())),
                )
                .nest("/settings", settings::global_service())
                .nest("/workers", workers::global_service())
                .nest("/service_logs", service_logs::global_service())
                .nest("/configs", configs::global_service())
                .nest("/scripts", scripts::global_service())
                .nest("/integrations", integration::global_service())
                .nest("/groups", groups::global_service())
                .nest("/flows", flows::global_service())
                .nest("/apps", apps::global_service().layer(cors.clone()))
                .nest("/schedules", schedule::global_service())
                .nest("/embeddings", embeddings::global_service())
                .route_layer(from_extractor::<ApiAuthed>())
                .route_layer(from_extractor::<users::Tokened>())
                .nest("/jobs", jobs::global_root_service())
                .nest(
                    "/srch/w/:workspace_id/index",
                    indexer_ee::workspaced_service(),
                )
                .nest("/srch/index", indexer_ee::global_service())
                .nest("/oidc", oidc_ee::global_service())
                .nest(
                    "/saml",
                    saml_ee::global_service().layer(Extension(Arc::clone(&sp_extension))),
                )
                .nest(
                    "/scim",
                    scim_ee::global_service()
                        .route_layer(axum::middleware::from_fn(has_scim_token)),
                )
                .nest("/concurrency_groups", concurrency_groups::global_service())
                .nest("/scripts_u", scripts::global_unauthed_service())
                .nest("/apps_u", {
                    #[cfg(feature = "enterprise")]
                    {
                        apps_ee::global_unauthed_service()
                    }

                    #[cfg(not(feature = "enterprise"))]
                    {
                        Router::new()
                    }
                })
                .nest(
                    "/w/:workspace_id/apps_u",
                    apps::unauthed_service()
                        .layer(from_extractor::<OptAuthed>())
                        .layer(cors.clone()),
                )
                .nest(
                    "/w/:workspace_id/jobs_u",
                    jobs::workspace_unauthed_service().layer(cors.clone()),
                )
                .route("/slack", post(slack_approvals::slack_app_callback_handler))
                .nest("/teams", {
                    #[cfg(feature = "enterprise")]
                    {
                        teams_ee::teams_service()
                    }

                    #[cfg(not(feature = "enterprise"))]
                    {
                        Router::new()
                    }
                })
                .route(
                    "/w/:workspace_id/jobs/slack_approval/:job_id",
                    get(slack_approvals::request_slack_approval),
                )
                .nest(
                    "/w/:workspace_id/resources_u",
                    resources::public_service().layer(cors.clone()),
                )
                .nest(
                    "/w/:workspace_id/capture_u",
                    capture::workspaced_unauthed_service().layer(cors.clone()),
                )
                .nest(
                    "/auth",
                    users::make_unauthed_service().layer(Extension(argon2)),
                )
                .nest("/oauth", {
                    #[cfg(feature = "oauth2")]
                    {
                        oauth2_ee::global_service().layer(Extension(Arc::clone(&sp_extension)))
                    }

                    #[cfg(not(feature = "oauth2"))]
                    Router::new()
                })
                .nest(
                    "/r",
                    {
                        #[cfg(feature = "http_trigger")]
                        {
                            http_triggers::routes_global_service()
                        }

                        #[cfg(not(feature = "http_trigger"))]
                        {
                            Router::new()
                        }
                    }
                    .layer(from_extractor::<OptAuthed>()),
                )
                .route("/version", get(git_v))
                .route("/uptodate", get(is_up_to_date))
                .route("/ee_license", get(ee_license))
                .route("/openapi.yaml", get(openapi))
                .route("/openapi.json", get(openapi_json)),
        )
        .fallback(static_assets::static_handler)
        .layer(middleware_stack);

    let app = if disable_response_logs {
        app
    } else {
        app.layer(
            TraceLayer::new_for_http()
                .on_response(MyOnResponse {})
                .make_span_with(MyMakeSpan {})
                .on_request(())
                .on_failure(MyOnFailure {}),
        )
    };

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let port = listener.local_addr().map(|x| x.port()).unwrap_or(8000);
    let ip = listener
        .local_addr()
        .map(|x| x.ip().to_string())
        .unwrap_or("localhost".to_string());

    let server = axum::serve(listener, app.into_make_service());

    tracing::info!(
        instance = %*INSTANCE_NAME,
        "server started on port={} and addr={}",
        port,
        ip
    );

    port_tx
        .send(format!("http://localhost:{}", port))
        .expect("Failed to send port");

    let server = server.with_graceful_shutdown(async move {
        rx.recv().await.ok();
        tracing::info!("Graceful shutdown of server");
    });

    server.await?;
    Ok(())
}

async fn is_up_to_date() -> Result<String, AppError> {
    let error_reading_version = || anyhow::anyhow!("Error reading latest released version");
    let version = HTTP_CLIENT
        .get("https://api.github.com/repos/windmill-labs/windmill/releases/latest")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("Impossible to reach api.github")?
        .json::<serde_json::Value>()
        .await?
        .get("tag_name")
        .ok_or_else(error_reading_version)?
        .as_str()
        .ok_or_else(error_reading_version)?
        .to_string();
    let release = GIT_VERSION
        .split('-')
        .next()
        .ok_or_else(error_reading_version)?
        .to_string();
    if version == release {
        Ok("yes".to_string())
    } else {
        Ok(format!("Update: {GIT_VERSION} -> {version}"))
    }
}

#[cfg(feature = "enterprise")]
async fn git_v() -> String {
    format!("EE {GIT_VERSION}")
}

#[cfg(not(feature = "enterprise"))]
async fn git_v() -> String {
    format!("CE {GIT_VERSION}")
}

#[cfg(not(feature = "enterprise"))]
async fn ee_license() -> &'static str {
    ""
}

#[cfg(feature = "enterprise")]
async fn ee_license() -> String {
    use windmill_common::ee::{LICENSE_KEY_ID, LICENSE_KEY_VALID};

    if *LICENSE_KEY_VALID.read().await {
        LICENSE_KEY_ID.read().await.clone()
    } else {
        "".to_string()
    }
}

async fn openapi() -> &'static str {
    include_str!("../openapi-deref.yaml")
}

async fn openapi_json() -> &'static str {
    include_str!("../openapi-deref.json")
}

pub async fn migrate_db(db: &DB) -> anyhow::Result<()> {
    db::migrate(db).await?;
    Ok(())
}
