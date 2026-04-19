//! Command dispatch logic.
//!
//! Bridges parsed CLI arguments to the corresponding `hermes_cli::*_cmd` handlers.

use hermes_cli::app::HermesApp;
use crate::commands::*;

/// Run the command dispatch loop.
pub fn dispatch_command(app: &HermesApp, command: Option<Commands>) -> anyhow::Result<()> {
    match command {
        Some(Commands::Chat { model, query, image, toolsets, skills, provider, resume, continue_last, worktree, checkpoints, max_turns, yolo, pass_session_id, source, quiet, verbose, skip_context_files, skip_memory, voice }) => {
            app.run_chat(model, query, image, toolsets, skills, provider, resume, continue_last, worktree, checkpoints, max_turns, yolo, pass_session_id, source, quiet, verbose, skip_context_files, skip_memory, voice)?;
        }
        Some(Commands::Setup { section, non_interactive, reset }) => {
            if reset {
                hermes_cli::setup_cmd::cmd_setup_reset()
                    .map_err(|e| anyhow::anyhow!(e))?;
            } else if let Some(sec) = section {
                hermes_cli::setup_cmd::cmd_setup_section(&sec, non_interactive)
                    .map_err(|e| anyhow::anyhow!(e))?;
            } else {
                hermes_cli::setup_cmd::cmd_setup()
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
        }
        Some(Commands::Backup { output, include_sessions, quick, label }) => {
            hermes_cli::backup_cmd::cmd_backup_extended(output.as_deref(), include_sessions, quick, label.as_deref())?;
        }
        Some(Commands::Restore { path, force }) => {
            hermes_cli::backup_cmd::cmd_restore(&path, force)?;
        }
        Some(Commands::BackupList) => {
            hermes_cli::backup_cmd::cmd_backup_list()?;
        }
        Some(Commands::Debug) => {
            hermes_cli::debug_cmd::cmd_debug()?;
        }
        Some(Commands::DebugShare { lines, expire_days, local_only }) => {
            hermes_cli::debug_share_cmd::cmd_debug_share(lines, expire_days, local_only)?;
        }
        Some(Commands::DebugDelete { url }) => {
            hermes_cli::debug_cmd::cmd_debug_delete(&url)?;
        }
        Some(Commands::Dump { session_id, show_keys }) => {
            match session_id {
                Some(sid) => {
                    hermes_cli::debug_cmd::cmd_dump_session(&sid, show_keys)?;
                }
                None => {
                    hermes_cli::dump_cmd::cmd_dump(show_keys)?;
                }
            }
        }
        Some(Commands::Tools { action }) => {
            match action {
                Some(ToolAction::List { platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_list(&platform)?;
                }
                Some(ToolAction::Info { name }) => {
                    hermes_cli::tools_cmd::cmd_tools_info(&name)?;
                }
                Some(ToolAction::Disable { names, platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_disable(&names, &platform)?;
                }
                Some(ToolAction::Enable { names, platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_enable(&names, &platform)?;
                }
                Some(ToolAction::DisableAll { platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_disable_all(&platform)?;
                }
                Some(ToolAction::EnableAll { platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_enable_all(&platform)?;
                }
                Some(ToolAction::DisableBatch { names, platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_disable_batch(&names, &platform)?;
                }
                Some(ToolAction::EnableBatch { names, platform }) => {
                    hermes_cli::tools_cmd::cmd_tools_enable_batch(&names, &platform)?;
                }
                Some(ToolAction::Summary) => {
                    hermes_cli::config_cmd::cmd_tools_summary()?;
                }
                None => {
                    hermes_cli::tools_cmd::cmd_tools_list("cli")?;
                }
            }
        }
        Some(Commands::Skills { action }) => {
            match action {
                Some(SkillAction::List { source }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("list", None, None, &source, 20, 1, "", false)?;
                }
                Some(SkillAction::Search { query, source, limit }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("search", None, Some(&query), &source, limit, 1, "", false)?;
                }
                Some(SkillAction::Browse { page, size, source }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("browse", None, None, &source, size, page, "", false)?;
                }
                Some(SkillAction::Install { identifier, category, force, yes }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills_install(&identifier, &category, force, yes)?;
                }
                Some(SkillAction::Inspect { identifier }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("inspect", Some(&identifier), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Info { name }) => app.show_skill_info(&name)?,
                Some(SkillAction::Enable { name, platform }) => app.enable_skill(&name, platform.as_deref())?,
                Some(SkillAction::Disable { name, platform }) => app.disable_skill(&name, platform.as_deref())?,
                Some(SkillAction::Uninstall { name }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("uninstall", Some(&name), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Check { name }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("check", name.as_deref(), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Update { name }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("update", name.as_deref(), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Audit { name }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("audit", name.as_deref(), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Commands) => app.list_skill_commands()?,
                Some(SkillAction::Publish { name, registry, repo }) => {
                    hermes_cli::skills_hub_cmd::cmd_skills_publish(&name, registry.as_deref(), repo.as_deref())?;
                }
                Some(SkillAction::Snapshot { snapshot_action }) => {
                    match snapshot_action {
                        Some(SnapshotAction::Export { output }) => {
                            hermes_cli::skills_hub_cmd::cmd_skills("snapshot-export", None, None, "all", 10, 1, output.as_deref().unwrap_or(""), false)?;
                        }
                        Some(SnapshotAction::Import { path, force }) => {
                            hermes_cli::skills_hub_cmd::cmd_skills("snapshot-import", Some(&path), None, "all", 10, 1, "", force)?;
                        }
                        None => {
                            hermes_cli::skills_hub_cmd::cmd_skills("snapshot-export", None, None, "all", 10, 1, "", false)?;
                        }
                    }
                }
                Some(SkillAction::Tap { tap_action }) => {
                    match tap_action {
                        Some(TapAction::List) => {
                            hermes_cli::skills_hub_cmd::cmd_skills("tap-list", None, None, "all", 10, 1, "", false)?;
                        }
                        Some(TapAction::Add { repo }) => {
                            hermes_cli::skills_hub_cmd::cmd_skills("tap-add", Some(&repo), None, "all", 10, 1, "", false)?;
                        }
                        Some(TapAction::Remove { name }) => {
                            hermes_cli::skills_hub_cmd::cmd_skills("tap-remove", Some(&name), None, "all", 10, 1, "", false)?;
                        }
                        None => {
                            hermes_cli::skills_hub_cmd::cmd_skills("tap-list", None, None, "all", 10, 1, "", false)?;
                        }
                    }
                }
                Some(SkillAction::Config) => {
                    hermes_cli::skills_hub_cmd::cmd_skills("config", None, None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Reset) => {
                    hermes_cli::skills_hub_cmd::cmd_skills_reset()?;
                }
                None => app.list_skills()?,
            }
        }
        Some(Commands::Gateway { action }) => {
            match action {
                Some(GatewayAction::Run { verbose, quiet, replace }) => {
                    app.run_gateway_with_opts(verbose, quiet, replace)?;
                }
                None => {
                    app.run_gateway_with_opts(false, false, false)?;
                }
                Some(GatewayAction::Start { all, system }) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_start(all, system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Stop { all, system }) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_stop(all, system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Restart { system, all }) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_restart(system, all)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Status { deep, system }) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_status(deep, system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Install { force, system, run_as_user }) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_install(force, system, run_as_user.as_deref())
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Uninstall { system }) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_uninstall(system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Setup) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_setup()
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::MigrateLegacy) => {
                    hermes_cli::gateway_mgmt::cmd_gateway_migrate_legacy()
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
            }
        }
        Some(Commands::Doctor { fix }) => {
            if fix {
                app.run_doctor_fix()?;
            } else {
                app.run_doctor()?;
            }
        }
        Some(Commands::Models) => {
            app.list_models()?;
        }
        Some(Commands::Profiles { action }) => {
            match action {
                Some(ProfileAction::List) => {
                    hermes_cli::profiles_cmd::cmd_profile_list()?;
                }
                Some(ProfileAction::Create { name, clone, clone_all, clone_from, no_alias }) => {
                    hermes_cli::profiles_cmd::cmd_profile_create(
                        &name, clone, clone_all, clone_from.as_deref(), no_alias,
                    )?;
                }
                Some(ProfileAction::Use { name }) => {
                    hermes_cli::profiles_cmd::cmd_profile_use(&name)?;
                }
                Some(ProfileAction::Delete { name, force, yes }) => {
                    hermes_cli::profiles_cmd::cmd_profile_delete(&name, force || yes)?;
                }
                Some(ProfileAction::Show { name }) => {
                    hermes_cli::profiles_cmd::cmd_profile_show(&name)?;
                }
                Some(ProfileAction::Alias { name, remove, alias_name }) => {
                    let target = alias_name.as_deref();
                    hermes_cli::profiles_cmd::cmd_profile_alias(&name, target, remove)?;
                }
                Some(ProfileAction::Rename { old_name, new_name }) => {
                    hermes_cli::profiles_cmd::cmd_profile_rename(&old_name, &new_name)?;
                }
                Some(ProfileAction::Export { name, output }) => {
                    hermes_cli::profiles_cmd::cmd_profile_export(&name, output.as_deref())?;
                }
                Some(ProfileAction::Import { path, name: profile_name }) => {
                    hermes_cli::profiles_cmd::cmd_profile_import(&path, profile_name.as_deref())?;
                }
                None => {
                    hermes_cli::profiles_cmd::cmd_profile_list()?;
                }
            }
        }
        Some(Commands::Sessions { action }) => {
            let db = hermes_state::SessionDB::open_default()?;
            match action {
                Some(SessionAction::List { limit, source }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_list(&db, limit, source.as_deref(), false)?;
                }
                Some(SessionAction::Delete { session_id, yes }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_delete(&db, &session_id, yes)?;
                }
                Some(SessionAction::Search { query, limit }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_search(&db, &query, limit)?;
                }
                Some(SessionAction::Stats { source }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_stats(&db, source.as_deref())?;
                }
                Some(SessionAction::Rename { session_id, title }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_rename(&db, &session_id, &title)?;
                }
                Some(SessionAction::Prune { older_than, source, yes }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_prune(&db, older_than, source.as_deref(), yes)?;
                }
                Some(SessionAction::Browse { source, limit }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_list(&db, limit, source.as_deref(), true)?;
                }
                Some(SessionAction::Export { path, source, session_id }) => {
                    hermes_cli::sessions_cmd::cmd_sessions_export(&db, &path, source.as_deref(), session_id.as_deref())?;
                }
                None => {
                    hermes_cli::sessions_cmd::cmd_sessions_list(&db, 20, None, false)?;
                }
            }
        }
        Some(Commands::Config { action }) => {
            match action {
                Some(ConfigAction::Show { verbose }) => {
                    hermes_cli::config_cmd::cmd_config_show(verbose)?;
                }
                Some(ConfigAction::Edit) => {
                    hermes_cli::config_cmd::cmd_config_edit()?;
                }
                Some(ConfigAction::Set { key, value }) => {
                    hermes_cli::config_cmd::cmd_config_set(&key, &value)?;
                }
                Some(ConfigAction::Path) => {
                    hermes_cli::config_cmd::cmd_config_path()?;
                }
                Some(ConfigAction::EnvPath) => {
                    hermes_cli::config_cmd::cmd_config_env_path()?;
                }
                Some(ConfigAction::Check) => {
                    hermes_cli::config_cmd::cmd_config_check()?;
                }
                Some(ConfigAction::Migrate) => {
                    hermes_cli::config_cmd::cmd_config_migrate()?;
                }
                None => {
                    hermes_cli::config_cmd::cmd_config_show(false)?;
                }
            }
        }
        Some(Commands::Batch { action }) => {
            match action {
                Some(BatchAction::Run { dataset, name, model, batch_size, workers, max_iterations, max_samples, resume, distribution }) => {
                    let opts = hermes_cli::batch_cmd::BatchRunOptions {
                        dataset,
                        run_name: name,
                        model,
                        batch_size: Some(batch_size),
                        workers: Some(workers),
                        max_iterations: Some(max_iterations),
                        max_samples: Some(max_samples),
                        resume,
                        distribution,
                    };
                    hermes_cli::batch_cmd::cmd_batch_run(&opts)?;
                }
                Some(BatchAction::Distributions) => {
                    hermes_cli::batch_cmd::cmd_batch_distributions()?;
                }
                Some(BatchAction::Status { name }) => {
                    hermes_cli::batch_cmd::cmd_batch_status(&name)?;
                }
                None => {
                    hermes_cli::batch_cmd::cmd_batch_distributions()?;
                }
            }
        }
        Some(Commands::Swe { action }) => {
            match action {
                Some(SweAction::Evaluate { dataset, split, sandbox, max_samples, output, model, quick }) => {
                    let opts = hermes_cli::swe_cmd::SweEvaluateOptions {
                        dataset,
                        split,
                        sandbox,
                        max_samples,
                        output_dir: output,
                        model,
                        quick,
                    };
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(hermes_cli::swe_cmd::cmd_swe_evaluate(&opts))?;
                }
                Some(SweAction::Benchmark { quick }) => {
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(hermes_cli::swe_cmd::cmd_swe_benchmark(quick))?;
                }
                Some(SweAction::Env) => {
                    hermes_cli::swe_cmd::cmd_swe_env_info()?;
                }
                None => {
                    hermes_cli::swe_cmd::cmd_swe_env_info()?;
                }
            }
        }
        Some(Commands::Cron { action }) => {
            match action {
                Some(CronAction::List { all }) => {
                    hermes_cli::cron_cmd::cmd_cron_list(all)?;
                }
                Some(CronAction::Create { name, schedule, command, prompt, delivery, paused, repeat, skill, script }) => {
                    hermes_cli::cron_cmd::cmd_cron_create(&name, &schedule, &command, prompt.as_deref(), &delivery.unwrap_or_else(|| "local".to_string()), !paused, repeat, skill.as_deref(), script.as_deref())?;
                }
                Some(CronAction::Delete { job_id, force }) => {
                    hermes_cli::cron_cmd::cmd_cron_delete(&job_id, force)?;
                }
                Some(CronAction::Pause { job_id }) => {
                    hermes_cli::cron_cmd::cmd_cron_pause(&job_id)?;
                }
                Some(CronAction::Resume { job_id }) => {
                    hermes_cli::cron_cmd::cmd_cron_resume(&job_id)?;
                }
                Some(CronAction::Edit { job_id, schedule, name, prompt, deliver, repeat, script, skill, add_skill, remove_skill, clear_skills }) => {
                    hermes_cli::cron_cmd::cmd_cron_edit(&job_id, schedule.as_deref(), name.as_deref(), prompt.as_deref(), deliver.as_deref(), repeat, script.as_deref(), skill.as_deref(), add_skill.as_deref(), remove_skill.as_deref(), clear_skills)?;
                }
                Some(CronAction::Run { job_id }) => {
                    hermes_cli::cron_cmd::cmd_cron_run(&job_id)?;
                }
                Some(CronAction::Status) => {
                    hermes_cli::cron_cmd::cmd_cron_status()?;
                }
                Some(CronAction::Tick) => {
                    hermes_cli::cron_cmd::cmd_cron_tick()?;
                }
                None => {
                    hermes_cli::cron_cmd::cmd_cron_list(false)?;
                }
            }
        }
        Some(Commands::Auth { action }) => {
            match action {
                Some(AuthAction::Add { provider, auth_type, api_key, label, client_id, no_browser, portal_url, inference_url, scope, timeout, insecure, ca_bundle }) => {
                    hermes_cli::auth_cmd::cmd_auth_add(
                        &provider,
                        &auth_type,
                        api_key.as_deref(),
                        label.as_deref(),
                        client_id.as_deref(),
                        no_browser,
                        portal_url.as_deref(),
                        inference_url.as_deref(),
                        scope.as_deref(),
                        timeout,
                        insecure,
                        ca_bundle.as_deref(),
                    )?;
                }
                Some(AuthAction::List { provider }) => {
                    hermes_cli::auth_cmd::cmd_auth_list(provider.as_deref())?;
                }
                Some(AuthAction::Remove { provider, target }) => {
                    hermes_cli::auth_cmd::cmd_auth_remove(&provider, &target)?;
                }
                Some(AuthAction::Reset { provider }) => {
                    hermes_cli::auth_cmd::cmd_auth_reset(&provider)?;
                }
                Some(AuthAction::Status) => {
                    hermes_cli::auth_cmd::cmd_auth_status()?;
                }
                None => {
                    hermes_cli::auth_cmd::cmd_auth_status()?;
                }
            }
        }
        Some(Commands::Status { all, deep }) => {
            hermes_cli::status_cmd::cmd_status(all, deep)?;
        }
        Some(Commands::Insights { days, source }) => {
            hermes_cli::insights_cmd::cmd_insights(days, source.as_deref())?;
        }
        Some(Commands::Completion { shell }) => {
            hermes_cli::completion_cmd::cmd_completion(&shell)?;
        }
        Some(Commands::Version) => {
            hermes_cli::version_cmd::cmd_version();
        }
        Some(Commands::Logs { log_name, lines, follow, level, session, component, since }) => {
            hermes_cli::logs_cmd::cmd_logs(
                log_name.as_deref().unwrap_or("agent"),
                lines,
                follow,
                level.as_deref(),
                session.as_deref(),
                component.as_deref(),
                since.as_deref(),
            )?;
        }
        Some(Commands::Webhook { action }) => {
            match action {
                WebhookAction::Subscribe { name, prompt, events, description, deliver, deliver_chat_id, skills, secret } => {
                    hermes_cli::webhook_cmd::cmd_webhook_subscribe(
                        &name, &prompt, &events, &description, &deliver, deliver_chat_id, &skills, secret.as_deref(),
                    )?;
                }
                WebhookAction::List => {
                    hermes_cli::webhook_cmd::cmd_webhook_list()?;
                }
                WebhookAction::Remove { name } => {
                    hermes_cli::webhook_cmd::cmd_webhook_remove(&name)?;
                }
                WebhookAction::Test { name, payload } => {
                    hermes_cli::webhook_cmd::cmd_webhook_test(&name, &payload)?;
                }
            }
        }
        Some(Commands::Plugins { action }) => {
            match action {
                Some(PluginAction::Install { identifier, force }) => {
                    hermes_cli::plugins_cmd::cmd_plugins_install(&identifier, force)?;
                }
                Some(PluginAction::Update { name }) => {
                    hermes_cli::plugins_cmd::cmd_plugins_update(&name)?;
                }
                Some(PluginAction::Remove { name }) => {
                    hermes_cli::plugins_cmd::cmd_plugins_remove(&name)?;
                }
                Some(PluginAction::List) | None => {
                    hermes_cli::plugins_cmd::cmd_plugins_list()?;
                }
                Some(PluginAction::Enable { name }) => {
                    hermes_cli::plugins_cmd::cmd_plugins_enable(&name)?;
                }
                Some(PluginAction::Disable { name }) => {
                    hermes_cli::plugins_cmd::cmd_plugins_disable(&name)?;
                }
            }
        }
        Some(Commands::Memory { action }) => {
            match action {
                Some(MemoryAction::Setup) => {
                    hermes_cli::memory_cmd::cmd_memory_setup()?;
                }
                Some(MemoryAction::Status) => {
                    hermes_cli::memory_cmd::cmd_memory_status()?;
                }
                Some(MemoryAction::Off) => {
                    hermes_cli::memory_cmd::cmd_memory_off()?;
                }
                None => {
                    hermes_cli::memory_cmd::cmd_memory_status()?;
                }
            }
        }
        Some(Commands::Logout { provider }) => {
            hermes_cli::auth_cmd::cmd_logout(provider.as_deref())?;
        }
        Some(Commands::Import { path, force }) => {
            hermes_cli::backup_cmd::cmd_import(&path, force)?;
        }
        Some(Commands::Mcp { action }) => {
            match action {
                Some(McpAction::List) => {
                    hermes_cli::mcp_cmd::cmd_mcp_list()?;
                }
                Some(McpAction::Add { name, url, command, args, auth, preset, env }) => {
                    hermes_cli::mcp_cmd::cmd_mcp_add(&name, url.as_deref(), command.as_deref(), &args, auth.as_deref(), preset.as_deref(), &env)?;
                }
                Some(McpAction::Remove { name }) => {
                    hermes_cli::mcp_cmd::cmd_mcp("remove", Some(&name), "", &[])?;
                }
                Some(McpAction::Test { name }) => {
                    hermes_cli::mcp_cmd::cmd_mcp("test", Some(&name), "", &[])?;
                }
                Some(McpAction::Configure { name }) => {
                    hermes_cli::mcp_cmd::cmd_mcp_configure(&name)?;
                }
                Some(McpAction::Serve { verbose }) => {
                    hermes_cli::mcp_cmd::cmd_mcp_serve(verbose)?;
                }
                None => {
                    hermes_cli::mcp_cmd::cmd_mcp_list()?;
                }
            }
        }
        None => {
            // Default: interactive chat
            app.run_chat(None, None, None, None, None, None, None, None, false, false, None, false, false, None, false, false, false, false, false)?;
        }
        Some(Commands::Model { action }) => {
            match action {
                Some(ModelAction::Browse) | Some(ModelAction::List) | None => {
                    hermes_cli::model_cmd::cmd_model()?;
                }
                Some(ModelAction::Switch { model }) => {
                    hermes_cli::model_cmd::cmd_model_switch(&model)?;
                }
                Some(ModelAction::Info { model }) => {
                    hermes_cli::model_cmd::cmd_model_info(&model)?;
                }
            }
        }
        Some(Commands::Skin { action }) => {
            match action {
                Some(SkinAction::List) | None => {
                    hermes_cli::skin_engine::cmd_skin_list()?;
                }
                Some(SkinAction::Apply { name }) => {
                    hermes_cli::skin_engine::cmd_skin_apply(&name)?;
                }
                Some(SkinAction::Preview { name }) => {
                    hermes_cli::skin_engine::cmd_skin_preview(&name)?;
                }
            }
        }
        Some(Commands::Login { provider, client_id, no_browser, scopes, portal_url, inference_url, timeout, ca_bundle, insecure }) => {
            hermes_cli::login_cmd::cmd_login(&provider, client_id.as_deref(), no_browser, scopes.as_deref(), portal_url.as_deref(), inference_url.as_deref(), timeout, ca_bundle.as_deref(), insecure)?;
        }
        Some(Commands::Pairing { action }) => {
            match action {
                PairingAction::List => {
                    hermes_cli::pairing_cmd::cmd_pairing_list()?;
                }
                PairingAction::Approve { platform, code } => {
                    hermes_cli::pairing_cmd::cmd_pairing_approve(&platform, &code)?;
                }
                PairingAction::Revoke { platform, code } => {
                    hermes_cli::pairing_cmd::cmd_pairing_revoke(&platform, &code)?;
                }
                PairingAction::ClearPending => {
                    hermes_cli::pairing_cmd::cmd_pairing_clear_pending()?;
                }
            }
        }
        Some(Commands::Update { preview, force, gateway }) => {
            hermes_cli::update_cmd::cmd_update(preview, force, gateway)?;
        }
        Some(Commands::Uninstall { keep_data, keep_config, yes }) => {
            hermes_cli::uninstall_cmd::cmd_uninstall(keep_data, keep_config, yes)?;
        }
        Some(Commands::Dashboard { port, host, no_open, insecure }) => {
            hermes_cli::dashboard_cmd::cmd_dashboard_with_opts(&host, port, no_open, insecure)?;
        }
        Some(Commands::WhatsApp { action, token, phone_id }) => {
            hermes_cli::whatsapp_cmd::cmd_whatsapp(&action, token.as_deref(), phone_id.as_deref())?;
        }
        Some(Commands::Acp { action, editor }) => {
            hermes_cli::acp_cmd::cmd_acp(action.as_deref().unwrap_or("status"), editor.as_deref())?;
        }
        Some(Commands::Claw { action, source, force, dry_run, preset, overwrite, migrate_secrets, yes, workspace_target, skill_conflict }) => {
            hermes_cli::claw_cmd::cmd_claw(&action, source.as_deref(), force, dry_run, &preset, overwrite, migrate_secrets, yes, workspace_target.as_deref(), &skill_conflict)?;
        }
    }

    Ok(())
}
