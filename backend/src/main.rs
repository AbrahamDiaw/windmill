/*
 * Author: Ruben Fiszel
 * Copyright: Windmill Labs, Inc 2022
 * This file and its contents are licensed under the AGPLv3 License.
 * Please see the included NOTICE for copyright information and
 * LICENSE-AGPL for a copy of the license.
 */

use anyhow::Context;
use monitor::{
    load_base_url, load_otel, reload_delete_logs_periodically_setting, reload_indexer_config,
    reload_instance_python_version_setting, reload_nuget_config_setting,
    reload_timeout_wait_result_setting, send_current_log_file_to_object_store,
    send_logs_to_object_store,
};
use rand::Rng;
use sqlx::{postgres::PgListener, Pool, Postgres};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    fs::{create_dir_all, DirBuilder, File},
    io::AsyncReadExt,
};
use uuid::Uuid;
use windmill_api::HTTP_CLIENT;

#[cfg(feature = "enterprise")]
use windmill_common::ee::{maybe_renew_license_key_on_start, LICENSE_KEY_ID, LICENSE_KEY_VALID};

use windmill_common::{
    global_settings::{
        BASE_URL_SETTING, BUNFIG_INSTALL_SCOPES_SETTING, CRITICAL_ALERT_MUTE_UI_SETTING,
        CRITICAL_ERROR_CHANNELS_SETTING, CUSTOM_TAGS_SETTING, DEFAULT_TAGS_PER_WORKSPACE_SETTING,
        DEFAULT_TAGS_WORKSPACES_SETTING, ENV_SETTINGS, EXPOSE_DEBUG_METRICS_SETTING,
        EXPOSE_METRICS_SETTING, EXTRA_PIP_INDEX_URL_SETTING, HUB_BASE_URL_SETTING, INDEXER_SETTING,
        INSTANCE_PYTHON_VERSION_SETTING, JOB_DEFAULT_TIMEOUT_SECS_SETTING, JWT_SECRET_SETTING,
        KEEP_JOB_DIR_SETTING, LICENSE_KEY_SETTING, MONITOR_LOGS_ON_OBJECT_STORE_SETTING,
        NPM_CONFIG_REGISTRY_SETTING, NUGET_CONFIG_SETTING, OAUTH_SETTING, OTEL_SETTING,
        PIP_INDEX_URL_SETTING, REQUEST_SIZE_LIMIT_SETTING,
        REQUIRE_PREEXISTING_USER_FOR_OAUTH_SETTING, RETENTION_PERIOD_SECS_SETTING,
        SAML_METADATA_SETTING, SCIM_TOKEN_SETTING, SMTP_SETTING, TEAMS_SETTING,
        TIMEOUT_WAIT_RESULT_SETTING,
    },
    scripts::ScriptLang,
    stats_ee::schedule_stats,
    utils::{hostname, rd_string, Mode, GIT_VERSION},
    worker::{reload_custom_tags_setting, HUB_CACHE_DIR, TMP_DIR, WORKER_GROUP},
    DB, METRICS_ENABLED,
};

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
use monitor::monitor_mem;

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[cfg(feature = "enterprise")]
use windmill_common::METRICS_ADDR;

#[cfg(feature = "parquet")]
use windmill_common::global_settings::OBJECT_STORE_CACHE_CONFIG_SETTING;

use windmill_worker::{
    get_hub_script_content_and_requirements, BUN_BUNDLE_CACHE_DIR, BUN_CACHE_DIR,
    BUN_DEPSTAR_CACHE_DIR, CSHARP_CACHE_DIR, DENO_CACHE_DIR, DENO_CACHE_DIR_DEPS,
    DENO_CACHE_DIR_NPM, GO_BIN_CACHE_DIR, GO_CACHE_DIR, LOCK_CACHE_DIR, PIP_CACHE_DIR,
    POWERSHELL_CACHE_DIR, PY310_CACHE_DIR, PY311_CACHE_DIR, PY312_CACHE_DIR, PY313_CACHE_DIR,
    RUST_CACHE_DIR, TAR_PIP_CACHE_DIR, TAR_PY310_CACHE_DIR, TAR_PY311_CACHE_DIR,
    TAR_PY312_CACHE_DIR, TAR_PY313_CACHE_DIR, TMP_LOGS_DIR, UV_CACHE_DIR,
};

use crate::monitor::{
    initial_load, load_keep_job_dir, load_metrics_debug_enabled, load_require_preexisting_user,
    load_tag_per_workspace_enabled, load_tag_per_workspace_workspaces, monitor_db,
    reload_base_url_setting, reload_bunfig_install_scopes_setting,
    reload_critical_alert_mute_ui_setting, reload_critical_error_channels_setting,
    reload_extra_pip_index_url_setting, reload_hub_base_url_setting,
    reload_job_default_timeout_setting, reload_jwt_secret_setting, reload_license_key,
    reload_npm_config_registry_setting, reload_pip_index_url_setting,
    reload_retention_period_setting, reload_scim_token_setting, reload_smtp_config,
    reload_worker_config,
};

#[cfg(feature = "parquet")]
use crate::monitor::reload_s3_cache_setting;

const DEFAULT_NUM_WORKERS: usize = 1;
const DEFAULT_PORT: u16 = 8000;
const DEFAULT_SERVER_BIND_ADDR: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);

mod ee;
mod monitor;

#[inline(always)]
fn create_and_run_current_thread_inner<F, R>(future: F) -> R
where
    F: std::future::Future<Output = R> + 'static,
    R: Send + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(32)
        .build()
        .unwrap();

    // Since this is the main future, we want to box it in debug mode because it tends to be fairly
    // large and the compiler won't optimize repeated copies. We also make this runtime factory
    // function #[inline(always)] to avoid holding the unboxed, unused future on the stack.
    #[cfg(debug_assertions)]
    // SAFETY: this this is guaranteed to be running on a current-thread executor
    let future = Box::pin(future);

    rt.block_on(future)
}

pub fn main() -> anyhow::Result<()> {
    #[cfg(feature = "deno_core")]
    deno_core::JsRuntime::init_platform(None, false);
    create_and_run_current_thread_inner(windmill_main())
}

async fn cache_hub_scripts(file_path: Option<String>) -> anyhow::Result<()> {
    let file_path = file_path.unwrap_or("./hubPaths.json".to_string());
    let mut file = File::open(&file_path)
        .await
        .with_context(|| format!("Could not open {}, make sure it exists", &file_path))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let paths = serde_json::from_str::<HashMap<String, String>>(&contents).with_context(|| {
        format!(
            "Could not parse {}, make sure it is a valid JSON object with string keys and values",
            &file_path
        )
    })?;

    create_dir_all(HUB_CACHE_DIR).await?;
    create_dir_all(BUN_BUNDLE_CACHE_DIR).await?;

    for path in paths.values() {
        tracing::info!("Caching hub script at {path}");
        let res = get_hub_script_content_and_requirements(Some(path), None).await?;
        if res
            .language
            .as_ref()
            .is_some_and(|x| x == &ScriptLang::Deno)
        {
            let job_dir = format!("{}/cache_init/{}", TMP_DIR, Uuid::new_v4());
            create_dir_all(&job_dir).await?;
            let _ = windmill_worker::generate_deno_lock(
                &Uuid::nil(),
                &res.content,
                &mut 0,
                &mut None,
                &job_dir,
                None,
                "global",
                "global",
                "",
                &mut None,
            )
            .await?;
            tokio::fs::remove_dir_all(job_dir).await?;
        } else if res.language.as_ref().is_some_and(|x| x == &ScriptLang::Bun) {
            let job_id = Uuid::new_v4();
            let job_dir = format!("{}/cache_init/{}", TMP_DIR, job_id);
            create_dir_all(&job_dir).await?;
            if let Some(lockfile) = res.lockfile {
                let _ = windmill_worker::prepare_job_dir(&lockfile, &job_dir).await?;
                let envs = windmill_worker::get_common_bun_proc_envs(None).await;
                let _ = windmill_worker::install_bun_lockfile(
                    &mut 0,
                    &mut None,
                    &job_id,
                    "admins",
                    None,
                    &job_dir,
                    "cache_init",
                    envs.clone(),
                    false,
                    &mut None,
                )
                .await?;

                let _ = windmill_common::worker::write_file(&job_dir, "main.js", &res.content)?;

                if let Err(e) = windmill_worker::prebundle_bun_script(
                    &res.content,
                    Some(&lockfile),
                    &path,
                    &job_id,
                    "admins",
                    None,
                    &job_dir,
                    "",
                    "cache_init",
                    "",
                    &mut None,
                )
                .await
                {
                    panic!("Error prebundling bun script: {e:#}");
                }
            } else {
                tracing::warn!("No lockfile found for bun script {path}, skipping...");
            }
            tokio::fs::remove_dir_all(job_dir).await?;
        }
    }
    Ok(())
}

async fn windmill_main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info")
    }

    let hostname = hostname();

    let mut enable_standalone_indexer: bool = false;

    let mode = std::env::var("MODE")
        .map(|x| x.to_lowercase())
        .map(|x| {
            if &x == "server" {
                println!("Binary is in 'server' mode");
                Mode::Server
            } else if &x == "worker" {
                tracing::info!("Binary is in 'worker' mode");
                #[cfg(windows)]
                {
                    println!("It is highly recommended to use the agent mode instead on windows (MODE=agent) and to pass a BASE_INTERNAL_URL");
                }
                Mode::Worker
            } else if &x == "agent" {
                println!("Binary is in 'agent' mode");
                if std::env::var("BASE_INTERNAL_URL").is_err() {
                    panic!("BASE_INTERNAL_URL is required in agent mode")
                }
                if std::env::var("JOB_TOKEN").is_err() {
                    println!("JOB_TOKEN is not passed, hence workers will still need to create permissions for each job and the DATABASE_URL needs to be of a role that can INSERT into the job_perms table")
                }

                #[cfg(not(feature = "enterprise"))]
                {
                    panic!("Agent mode is only available in the EE, ignoring...");
                }
                #[cfg(feature = "enterprise")]
                Mode::Agent
            } else if &x == "indexer" {
                tracing::info!("Binary is in 'indexer' mode");
                #[cfg(not(feature = "tantivy"))]
                {
                    eprintln!("Cannot start the indexer because tantivy is not included in this binary/image. Make sure you are using the EE image if you want to access the full text search features.");
                    panic!("Indexer mode requires compiling with the tantivy feature flag.");
                }
                #[cfg(feature = "tantivy")]
                Mode::Indexer
            } else if &x == "standalone+search"{
                    enable_standalone_indexer = true;
                    println!("Binary is in 'standalone' mode with search enabled");
                    Mode::Standalone
            }
            else {
                if &x != "standalone" {
                    eprintln!("mode not recognized, defaulting to standalone: {x}");
                } else {
                    println!("Binary is in 'standalone' mode");
                }
                Mode::Standalone
            }
        })
        .unwrap_or_else(|_| {
            tracing::info!("Mode not specified, defaulting to standalone");
            Mode::Standalone
        });

    #[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
    println!("jemalloc enabled");

    let cli_arg = std::env::args().nth(1).unwrap_or_default();

    match cli_arg.as_str() {
        "cache" => {
            #[cfg(feature = "embedding")]
            {
                println!("Caching embedding model...");
                windmill_api::embeddings::ModelInstance::load_model_files().await?;
                println!("Cached embedding model");
            }
            #[cfg(not(feature = "embedding"))]
            {
                println!("Embeddings are not enabled, ignoring...");
            }

            cache_hub_scripts(std::env::args().nth(2)).await?;

            return Ok(());
        }
        "-v" | "--version" | "version" => {
            println!("Windmill {}", GIT_VERSION);
            return Ok(());
        }
        _ => {}
    }

    #[allow(unused_mut)]
    let mut num_workers = if mode == Mode::Server || mode == Mode::Indexer {
        0
    } else {
        std::env::var("NUM_WORKERS")
            .ok()
            .and_then(|x| x.parse::<i32>().ok())
            .unwrap_or(DEFAULT_NUM_WORKERS as i32)
    };

    if num_workers > 1 {
        println!(
            "We STRONGLY recommend using at most 1 worker per container, use at your own risks"
        );
    }

    let server_mode = !std::env::var("DISABLE_SERVER")
        .ok()
        .and_then(|x| x.parse::<bool>().ok())
        .unwrap_or(false)
        && (mode == Mode::Server || mode == Mode::Standalone);

    let indexer_mode = mode == Mode::Indexer;

    let server_bind_address: IpAddr = if server_mode || indexer_mode {
        std::env::var("SERVER_BIND_ADDR")
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(IpAddr::from(DEFAULT_SERVER_BIND_ADDR))
    } else {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    };

    println!("Connecting to database...");
    let db = windmill_common::connect_db(server_mode, indexer_mode).await?;

    load_otel(&db).await;

    tracing::info!("Database connected");

    let environment = load_base_url(&db)
        .await
        .unwrap_or_else(|_| "local".to_string())
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(".")
        .next()
        .unwrap_or_else(|| "local")
        .to_string();

    let _guard = windmill_common::tracing_init::initialize_tracing(&hostname, &mode, &environment);

    let num_version = sqlx::query_scalar!("SELECT version()").fetch_one(&db).await;

    tracing::info!(
        "PostgreSQL version: {} (windmill require PG >= 14)",
        num_version
            .ok()
            .flatten()
            .unwrap_or_else(|| "UNKNOWN".to_string())
    );

    let is_agent = mode == Mode::Agent;

    if !is_agent {
        let skip_migration = std::env::var("SKIP_MIGRATION")
            .map(|val| val == "true")
            .unwrap_or(false);

        if !skip_migration {
            // migration code to avoid break
            windmill_api::migrate_db(&db).await?;
        } else {
            tracing::info!("SKIP_MIGRATION set, skipping db migration...")
        }
    }

    let (killpill_tx, mut killpill_rx) = tokio::sync::broadcast::channel::<()>(2);
    let mut monitor_killpill_rx = killpill_tx.subscribe();
    let server_killpill_rx = killpill_tx.subscribe();
    let (killpill_phase2_tx, _killpill_phase2_rx) = tokio::sync::broadcast::channel::<()>(2);

    let shutdown_signal =
        windmill_common::shutdown_signal(killpill_tx.clone(), killpill_tx.subscribe());

    #[cfg(feature = "enterprise")]
    tracing::info!(
        "
##############################
Windmill Enterprise Edition {GIT_VERSION}
##############################"
    );

    #[cfg(not(feature = "enterprise"))]
    tracing::info!(
        "
##############################
Windmill Community Edition {GIT_VERSION}
##############################"
    );

    display_config(&ENV_SETTINGS);

    if let Err(e) = reload_base_url_setting(&db).await {
        tracing::error!("Error loading base url: {:?}", e)
    }

    if let Err(e) = reload_critical_error_channels_setting(&db).await {
        tracing::error!("Could loading critical error emails setting: {:?}", e);
    }

    #[cfg(feature = "enterprise")]
    {
        // load the license key and check if it's valid
        // if not valid and not server mode just quit
        // if not expired and server mode then force renewal
        // if key still invalid and num_workers > 0, set to 0
        if let Err(err) = reload_license_key(&db).await {
            tracing::error!("Failed to reload license key: {err:#}");
        }
        let valid_key = *LICENSE_KEY_VALID.read().await;
        if !valid_key && !server_mode {
            tracing::error!("Invalid license key, workers require a valid license key");
        }
        if server_mode {
            // only force renewal if invalid but not empty (= expired)
            let renewed_now = maybe_renew_license_key_on_start(
                &HTTP_CLIENT,
                &db,
                !valid_key && !LICENSE_KEY_ID.read().await.is_empty(),
            )
            .await;
            if renewed_now {
                if let Err(err) = reload_license_key(&db).await {
                    tracing::error!("Failed to reload license key: {err:#}");
                }
            }
        }
    }

    let worker_mode = num_workers > 0;

    if server_mode || worker_mode || indexer_mode {
        let port_var = std::env::var("PORT").ok().and_then(|x| x.parse().ok());

        let port = if server_mode || indexer_mode {
            port_var.unwrap_or(DEFAULT_PORT as u16)
        } else {
            port_var.unwrap_or(0)
        };

        let default_base_internal_url = format!("http://localhost:{}", port.to_string());
        // since it's only on server mode, the port is statically defined
        let base_internal_url: String = if let Ok(base_url) = std::env::var("BASE_INTERNAL_URL") {
            if !is_agent {
                tracing::warn!("BASE_INTERNAL_URL is now unecessary and ignored unless the mode is 'agent', you can remove it.");
                default_base_internal_url.clone()
            } else {
                base_url
            }
        } else {
            default_base_internal_url.clone()
        };

        initial_load(&db, killpill_tx.clone(), worker_mode, server_mode, is_agent).await;

        monitor_db(
            &db,
            &base_internal_url,
            server_mode,
            worker_mode,
            true,
            killpill_tx.clone(),
        )
        .await;

        #[cfg(feature = "prometheus")]
        crate::monitor::monitor_pool(&db).await;

        send_logs_to_object_store(&db, &hostname, &mode);

        #[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
        if !worker_mode {
            monitor_mem().await;
        }

        let addr = SocketAddr::from((server_bind_address, port));

        let (base_internal_tx, base_internal_rx) = tokio::sync::oneshot::channel::<String>();

        DirBuilder::new()
            .recursive(true)
            .create("/tmp/windmill")
            .await
            .expect("could not create initial server dir");

        #[cfg(feature = "tantivy")]
        let should_index_jobs =
            mode == Mode::Indexer || (enable_standalone_indexer && mode == Mode::Standalone);

        reload_indexer_config(&db).await;

        #[cfg(feature = "tantivy")]
        let (index_reader, index_writer) = if should_index_jobs {
            let mut indexer_rx = killpill_rx.resubscribe();

            let (mut reader, mut writer) = (None, None);
            tokio::select! {
                _ = indexer_rx.recv() => {
                    tracing::info!("Received killpill, aborting index initialization");
                },
                res = windmill_indexer::completed_runs_ee::init_index(&db) => {
                        let res = res?;
                        reader = Some(res.0);
                        writer = Some(res.1);
                }

            }
            (reader, writer)
        } else {
            (None, None)
        };

        #[cfg(feature = "tantivy")]
        let indexer_f = {
            let indexer_rx = killpill_rx.resubscribe();
            let index_writer2 = index_writer.clone();
            async {
                if let Some(index_writer) = index_writer2 {
                    windmill_indexer::completed_runs_ee::run_indexer(
                        db.clone(),
                        index_writer,
                        indexer_rx,
                    )
                    .await?;
                }
                Ok(())
            }
        };

        #[cfg(all(feature = "tantivy", feature = "parquet"))]
        let (log_index_reader, log_index_writer) = if should_index_jobs {
            let mut indexer_rx = killpill_rx.resubscribe();

            let (mut reader, mut writer) = (None, None);
            tokio::select! {
                _ = indexer_rx.recv() => {
                    tracing::info!("Received killpill, aborting index initialization");
                },
                res = windmill_indexer::service_logs_ee::init_index(&db, killpill_tx.clone()) => {
                        let res = res?;
                        reader = Some(res.0);
                        writer = Some(res.1);
                }

            }
            (reader, writer)
        } else {
            (None, None)
        };

        #[cfg(all(feature = "tantivy", feature = "parquet"))]
        let log_indexer_f = {
            let log_indexer_rx = killpill_rx.resubscribe();
            let log_index_writer2 = log_index_writer.clone();
            async {
                if let Some(log_index_writer) = log_index_writer2 {
                    windmill_indexer::service_logs_ee::run_indexer(
                        db.clone(),
                        log_index_writer,
                        log_indexer_rx,
                    )
                    .await?;
                }
                Ok(())
            }
        };

        #[cfg(not(feature = "tantivy"))]
        let index_reader = None;

        #[cfg(not(feature = "tantivy"))]
        let indexer_f = async { Ok(()) as anyhow::Result<()> };

        #[cfg(not(all(feature = "tantivy", feature = "parquet")))]
        let log_index_reader = None;

        #[cfg(not(all(feature = "tantivy", feature = "parquet")))]
        let log_indexer_f = async { Ok(()) as anyhow::Result<()> };

        let server_f = async {
            if !is_agent {
                windmill_api::run_server(
                    db.clone(),
                    index_reader,
                    log_index_reader,
                    addr,
                    server_killpill_rx,
                    base_internal_tx,
                    server_mode,
                    #[cfg(feature = "smtp")]
                    base_internal_url.clone(),
                )
                .await?;
            } else {
                base_internal_tx
                    .send(base_internal_url.clone())
                    .map_err(|e| {
                        anyhow::anyhow!("Could not send base_internal_url to agent: {e:#}")
                    })?;
            }
            Ok(()) as anyhow::Result<()>
        };

        let workers_f = async {
            let mut rx = killpill_rx.resubscribe();

            if !killpill_rx.try_recv().is_ok() {
                let base_internal_url = base_internal_rx.await?;
                if worker_mode {
                    run_workers(
                        db.clone(),
                        rx,
                        killpill_tx.clone(),
                        num_workers,
                        base_internal_url.clone(),
                        mode.clone() == Mode::Agent,
                        hostname.clone(),
                    )
                    .await?;
                    tracing::info!("All workers exited.");
                    killpill_tx.send(())?;
                } else {
                    rx.recv().await?;
                }
            }
            if killpill_phase2_tx.receiver_count() > 0 {
                tracing::info!("Starting phase 2 of shutdown");
                killpill_phase2_tx.send(())?;
                tracing::info!("Phase 2 of shutdown completed");
            }
            Ok(())
        };

        let monitor_f = async {
            let db = db.clone();
            let tx = killpill_tx.clone();

            let base_internal_url = base_internal_url.to_string();
            let h = tokio::spawn(async move {
                let mut listener = retry_listen_pg(&db).await;

                loop {
                    tokio::select! {
                        biased;
                        _ = monitor_killpill_rx.recv() => {
                            tracing::info!("received killpill for monitor job");
                            break;
                        },
                        _ = tokio::time::sleep(Duration::from_secs(30))    => {
                            monitor_db(
                                &db,
                                &base_internal_url,
                                server_mode,
                                worker_mode,
                                false,
                                tx.clone(),
                            )
                            .await;
                        },
                        notification = listener.recv() => {
                            match notification {
                                Ok(n) => {
                                    tracing::info!("Received new pg notification: {n:?}");
                                    match n.channel() {
                                        "notify_config_change" => {
                                            match n.payload() {
                                                "server" if server_mode => {
                                                    tracing::error!("Server config change detected but server config is obsolete: {}", n.payload());
                                                },
                                                a@ _ if worker_mode && a == format!("worker__{}", *WORKER_GROUP) => {
                                                    tracing::info!("Worker config change detected: {}", n.payload());
                                                    reload_worker_config(&db, tx.clone(), true).await;
                                                },
                                                _ => {
                                                    tracing::debug!("config changed but did not target this server/worker");
                                                }
                                            }
                                        },
                                        "notify_global_setting_change" => {
                                            tracing::info!("Global setting change detected: {}", n.payload());
                                            match n.payload() {
                                                BASE_URL_SETTING => {
                                                    if let Err(e) = reload_base_url_setting(&db).await {
                                                        tracing::error!(error = %e, "Could not reload base url setting");
                                                    }
                                                },
                                                OAUTH_SETTING => {
                                                    if let Err(e) = reload_base_url_setting(&db).await {
                                                        tracing::error!(error = %e, "Could not reload oauth setting");
                                                    }
                                                },
                                                CUSTOM_TAGS_SETTING => {
                                                    if let Err(e) = reload_custom_tags_setting(&db).await {
                                                        tracing::error!(error = %e, "Could not reload custom tags setting");
                                                    }
                                                },
                                                LICENSE_KEY_SETTING => {
                                                    if let Err(e) = reload_license_key(&db).await {
                                                        tracing::error!("Failed to reload license key: {e:#}");
                                                    }
                                                },
                                                DEFAULT_TAGS_PER_WORKSPACE_SETTING => {
                                                    if let Err(e) = load_tag_per_workspace_enabled(&db).await {
                                                        tracing::error!("Error loading default tag per workspace: {e:#}");
                                                    }
                                                },
                                                DEFAULT_TAGS_WORKSPACES_SETTING => {
                                                    if let Err(e) = load_tag_per_workspace_workspaces(&db).await {
                                                        tracing::error!("Error loading default tag per workspace workspaces: {e:#}");
                                                    }
                                                }
                                                SMTP_SETTING => {
                                                    reload_smtp_config(&db).await;
                                                },
                                                TEAMS_SETTING => {
                                                    tracing::info!("Teams setting changed.");
                                                },
                                                INDEXER_SETTING => {
                                                    reload_indexer_config(&db).await;
                                                },
                                                TIMEOUT_WAIT_RESULT_SETTING => {
                                                    reload_timeout_wait_result_setting(&db).await
                                                },
                                                RETENTION_PERIOD_SECS_SETTING => {
                                                    reload_retention_period_setting(&db).await
                                                },
                                                MONITOR_LOGS_ON_OBJECT_STORE_SETTING => {
                                                    reload_delete_logs_periodically_setting(&db).await
                                                },
                                                JOB_DEFAULT_TIMEOUT_SECS_SETTING => {
                                                    reload_job_default_timeout_setting(&db).await
                                                },
                                                #[cfg(feature = "parquet")]
                                                OBJECT_STORE_CACHE_CONFIG_SETTING if !is_agent => {
                                                    reload_s3_cache_setting(&db).await
                                                },
                                                SCIM_TOKEN_SETTING => {
                                                    reload_scim_token_setting(&db).await
                                                },
                                                EXTRA_PIP_INDEX_URL_SETTING => {
                                                    reload_extra_pip_index_url_setting(&db).await
                                                },
                                                PIP_INDEX_URL_SETTING => {
                                                    reload_pip_index_url_setting(&db).await
                                                },
                                                INSTANCE_PYTHON_VERSION_SETTING => {
                                                    reload_instance_python_version_setting(&db).await
                                                },
                                                NPM_CONFIG_REGISTRY_SETTING => {
                                                    reload_npm_config_registry_setting(&db).await
                                                },
                                                BUNFIG_INSTALL_SCOPES_SETTING => {
                                                    reload_bunfig_install_scopes_setting(&db).await
                                                },
                                                NUGET_CONFIG_SETTING => {
                                                    reload_nuget_config_setting(&db).await
                                                },
                                                KEEP_JOB_DIR_SETTING => {
                                                    load_keep_job_dir(&db).await;
                                                },
                                                REQUIRE_PREEXISTING_USER_FOR_OAUTH_SETTING => {
                                                    load_require_preexisting_user(&db).await;
                                                },
                                                EXPOSE_METRICS_SETTING  => {
                                                    tracing::info!("Metrics setting changed, restarting");
                                                    // we wait a bit randomly to avoid having all servers and workers shutdown at same time
                                                    let rd_delay = rand::rng().random_range(0..40);
                                                    tokio::time::sleep(Duration::from_secs(rd_delay)).await;
                                                    if let Err(e) = tx.send(()) {
                                                        tracing::error!(error = %e, "Could not send killpill to server");
                                                    }
                                                },
                                                EXPOSE_DEBUG_METRICS_SETTING => {
                                                    if let Err(e) = load_metrics_debug_enabled(&db).await {
                                                        tracing::error!(error = %e, "Could not reload debug metrics setting");
                                                    }
                                                },
                                                OTEL_SETTING => {
                                                    tracing::info!("OTEL setting changed, restarting");
                                                    // we wait a bit randomly to avoid having all servers and workers shutdown at same time
                                                    let rd_delay = rand::rng().random_range(0..4);
                                                    tokio::time::sleep(Duration::from_secs(rd_delay)).await;
                                                    if let Err(e) = tx.send(()) {
                                                        tracing::error!(error = %e, "Could not send killpill");
                                                    }
                                                },
                                                REQUEST_SIZE_LIMIT_SETTING => {
                                                    if server_mode {
                                                        tracing::info!("Request limit size change detected, killing server expecting to be restarted");
                                                        // we wait a bit randomly to avoid having all servers shutdown at same time
                                                        let rd_delay = rand::rng().random_range(0..4);
                                                        tokio::time::sleep(Duration::from_secs(rd_delay)).await;
                                                        if let Err(e) = tx.send(()) {
                                                            tracing::error!(error = %e, "Could not send killpill to server");
                                                        }
                                                    }
                                                },
                                                SAML_METADATA_SETTING => {
                                                    tracing::info!("SAML metadata change detected, killing server expecting to be restarted");
                                                    if let Err(e) = tx.send(()) {
                                                        tracing::error!(error = %e, "Could not send killpill to server");
                                                    }
                                                },
                                                HUB_BASE_URL_SETTING => {
                                                    if let Err(e) = reload_hub_base_url_setting(&db, server_mode).await {
                                                        tracing::error!(error = %e, "Could not reload hub base url setting");
                                                    }
                                                },
                                                CRITICAL_ERROR_CHANNELS_SETTING => {
                                                    if let Err(e) = reload_critical_error_channels_setting(&db).await {
                                                        tracing::error!(error = %e, "Could not reload critical error emails setting");
                                                    }
                                                },
                                                JWT_SECRET_SETTING => {
                                                    if let Err(e) = reload_jwt_secret_setting(&db).await {
                                                        tracing::error!(error = %e, "Could not reload jwt secret setting");
                                                    }
                                                },
                                                CRITICAL_ALERT_MUTE_UI_SETTING => {
                                                    tracing::info!("Critical alert UI setting changed");
                                                    if let Err(e) = reload_critical_alert_mute_ui_setting(&db).await {
                                                        tracing::error!(error = %e, "Could not reload critical alert UI setting");
                                                    }
                                                },
                                                a @_ => {
                                                    tracing::info!("Unrecognized Global Setting Change Payload: {:?}", a);
                                                }
                                            }
                                        },
                                        _ => {
                                            tracing::warn!("Unknown notification received");
                                            continue;
                                        }
                                    }
                                },
                                Err(e) => {
                                    tracing::error!(error = %e, "Could not receive notification, attempting to reconnect listener");
                                    tokio::select! {
                                        biased;
                                        _ = monitor_killpill_rx.recv() => {
                                            tracing::info!("received killpill for monitor job");
                                            break;
                                        },
                                        new_listener = retry_listen_pg(&db) => {
                                            listener = new_listener;
                                            continue;
                                        }
                                    }
                                }
                            };
                        }
                    }
                }
            });

            if let Err(e) = h.await {
                tracing::error!("Error waiting for monitor handle: {e:#}")
            }
            tracing::info!("Monitor exited");
            Ok(()) as anyhow::Result<()>
        };

        let metrics_f = async {
            if METRICS_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
                #[cfg(not(feature = "enterprise"))]
                tracing::error!("Metrics are only available in the EE, ignoring...");

                #[cfg(feature = "enterprise")]
                windmill_common::serve_metrics(*METRICS_ADDR, _killpill_phase2_rx, num_workers > 0)
                    .await;
            }
            Ok(()) as anyhow::Result<()>
        };

        if server_mode {
            schedule_stats(&db, &HTTP_CLIENT).await;
        }

        futures::try_join!(
            shutdown_signal,
            workers_f,
            monitor_f,
            server_f,
            metrics_f,
            indexer_f,
            log_indexer_f
        )?;
    } else {
        tracing::info!("Nothing to do, exiting.");
    }
    send_current_log_file_to_object_store(&db, &hostname, &mode).await;

    tracing::info!("Exiting connection pool");
    tokio::select! {
        _ = db.close() => {
            tracing::info!("Database connection pool closed");
        },
        _ = tokio::time::sleep(Duration::from_secs(15)) => {
            tracing::warn!("Could not close database connection pool in time (15s). Exiting anyway.");
        }
    }
    Ok(())
}

async fn listen_pg(db: &DB) -> Option<PgListener> {
    let mut listener = match PgListener::connect_with(&db).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "Could not connect to database");
            return None;
        }
    };

    if let Err(e) = listener
        .listen_all(vec!["notify_config_change", "notify_global_setting_change"])
        .await
    {
        tracing::error!(error = %e, "Could not listen to database");
        return None;
    }

    return Some(listener);
}

async fn retry_listen_pg(db: &DB) -> PgListener {
    let mut listener = listen_pg(db).await;
    loop {
        if listener.is_none() {
            tracing::info!("Retrying listening to pg listen in 5 seconds");
            tokio::time::sleep(Duration::from_secs(5)).await;
            listener = listen_pg(db).await;
        } else {
            tracing::info!("Successfully connected to pg listen");
            return listener.unwrap();
        }
    }
}

fn display_config(envs: &[&str]) {
    tracing::info!(
        "config: {}",
        envs.iter()
            .filter(|env| std::env::var(env).is_ok())
            .map(|env| {
                format!(
                    "{}: {}",
                    env,
                    std::env::var(env).unwrap_or_else(|_| "not set".to_string())
                )
            })
            .collect::<Vec<String>>()
            .join(", ")
    )
}

pub async fn run_workers(
    db: Pool<Postgres>,
    mut rx: tokio::sync::broadcast::Receiver<()>,
    tx: tokio::sync::broadcast::Sender<()>,
    num_workers: i32,
    base_internal_url: String,
    agent_mode: bool,
    hostname: String,
) -> anyhow::Result<()> {
    let mut killpill_rxs = vec![];
    for _ in 0..num_workers {
        killpill_rxs.push(rx.resubscribe());
    }

    if rx.try_recv().is_ok() {
        tracing::info!("Received killpill, exiting");
        return Ok(());
    }
    let instance_name = hostname
        .clone()
        .replace(" ", "")
        .split("-")
        .last()
        .unwrap()
        .to_ascii_lowercase()
        .to_string();

    // #[cfg(tokio_unstable)]
    // let monitor = tokio_metrics::TaskMonitor::new();

    let ip = windmill_common::external_ip::get_ip()
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = e.to_string(), "failed to get external IP");
            "unretrievable IP".to_string()
        });

    let mut handles = Vec::with_capacity(num_workers as usize);

    for x in [
        LOCK_CACHE_DIR,
        TMP_LOGS_DIR,
        UV_CACHE_DIR,
        TAR_PIP_CACHE_DIR,
        DENO_CACHE_DIR,
        DENO_CACHE_DIR_DEPS,
        DENO_CACHE_DIR_NPM,
        BUN_CACHE_DIR,
        PY310_CACHE_DIR,
        PY311_CACHE_DIR,
        PY312_CACHE_DIR,
        PY313_CACHE_DIR,
        TAR_PY310_CACHE_DIR,
        TAR_PY311_CACHE_DIR,
        TAR_PY312_CACHE_DIR,
        TAR_PY313_CACHE_DIR,
        PIP_CACHE_DIR,
        BUN_DEPSTAR_CACHE_DIR,
        BUN_BUNDLE_CACHE_DIR,
        GO_CACHE_DIR,
        GO_BIN_CACHE_DIR,
        RUST_CACHE_DIR,
        CSHARP_CACHE_DIR,
        HUB_CACHE_DIR,
        POWERSHELL_CACHE_DIR,
    ] {
        DirBuilder::new()
            .recursive(true)
            .create(x)
            .await
            .expect("could not create initial worker dir");
    }

    tracing::info!(
        "Starting {num_workers} workers and SLEEP_QUEUE={}ms",
        *windmill_worker::SLEEP_QUEUE
    );
    for i in 1..(num_workers + 1) {
        let db1 = db.clone();
        let instance_name = instance_name.clone();
        let worker_name = format!("wk-{}-{}-{}", *WORKER_GROUP, &instance_name, rd_string(5));
        let ip = ip.clone();
        let rx = killpill_rxs.pop().unwrap();
        let tx = tx.clone();
        let base_internal_url = base_internal_url.clone();
        let hostname = hostname.clone();

        handles.push(tokio::spawn(async move {
            if num_workers > 1 {
                tracing::info!(worker = %worker_name, "starting worker {i}");
            }

            let f = windmill_worker::run_worker(
                &db1,
                &hostname,
                worker_name,
                i as u64,
                num_workers as u32,
                &ip,
                rx,
                tx,
                &base_internal_url,
                agent_mode,
            );

            // #[cfg(tokio_unstable)]
            // {
            //     monitor.monitor(f, "worker").await
            // }

            // #[cfg(not(tokio_unstable))]
            // {
            f.await
            // }
        }));
    }

    futures::future::try_join_all(handles).await?;
    Ok(())
}
