//! Command dispatch logic.
//!
//! Bridges parsed CLI arguments to the corresponding `hermez_cli::*_cmd` handlers.

use hermez_cli::app::HermezApp;
use crate::commands::*;

/// Run the command dispatch loop.
pub fn dispatch_command(app: &HermezApp, command: Option<Commands>) -> anyhow::Result<()> {
    match command {
        Some(Commands::Chat { model, query, image, toolsets, skills, provider, resume, continue_last, worktree, checkpoints, max_turns, yolo, pass_session_id, source, quiet, verbose, skip_context_files, skip_memory, voice }) => {
            app.run_chat(model, query, image, toolsets, skills, provider, resume, continue_last, worktree, checkpoints, max_turns, yolo, pass_session_id, source, quiet, verbose, skip_context_files, skip_memory, voice)?;
        }
        Some(Commands::Setup { section, non_interactive, reset }) => {
            if reset {
                hermez_cli::setup_cmd::cmd_setup_reset()
                    .map_err(|e| anyhow::anyhow!(e))?;
            } else if let Some(sec) = section {
                hermez_cli::setup_cmd::cmd_setup_section(&sec, non_interactive)
                    .map_err(|e| anyhow::anyhow!(e))?;
            } else {
                hermez_cli::setup_cmd::cmd_setup()
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
        }
        Some(Commands::Backup { output, include_sessions, quick, label }) => {
            hermez_cli::backup_cmd::cmd_backup_extended(output.as_deref(), include_sessions, quick, label.as_deref())?;
        }
        Some(Commands::Restore { path, force }) => {
            hermez_cli::backup_cmd::cmd_restore(&path, force)?;
        }
        Some(Commands::BackupList) => {
            hermez_cli::backup_cmd::cmd_backup_list()?;
        }
        Some(Commands::Debug) => {
            hermez_cli::debug_cmd::cmd_debug()?;
        }
        Some(Commands::DebugShare { lines, expire_days, local_only }) => {
            hermez_cli::debug_share_cmd::cmd_debug_share(lines, expire_days, local_only)?;
        }
        Some(Commands::DebugDelete { url }) => {
            hermez_cli::debug_cmd::cmd_debug_delete(&url)?;
        }
        Some(Commands::Dump { session_id, show_keys }) => {
            match session_id {
                Some(sid) => {
                    hermez_cli::debug_cmd::cmd_dump_session(&sid, show_keys)?;
                }
                None => {
                    hermez_cli::dump_cmd::cmd_dump(show_keys)?;
                }
            }
        }
        Some(Commands::Tools { action }) => {
            match action {
                Some(ToolAction::List { platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_list(&platform)?;
                }
                Some(ToolAction::Info { name }) => {
                    hermez_cli::tools_cmd::cmd_tools_info(&name)?;
                }
                Some(ToolAction::Disable { names, platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_disable(&names, &platform)?;
                }
                Some(ToolAction::Enable { names, platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_enable(&names, &platform)?;
                }
                Some(ToolAction::DisableAll { platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_disable_all(&platform)?;
                }
                Some(ToolAction::EnableAll { platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_enable_all(&platform)?;
                }
                Some(ToolAction::DisableBatch { names, platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_disable_batch(&names, &platform)?;
                }
                Some(ToolAction::EnableBatch { names, platform }) => {
                    hermez_cli::tools_cmd::cmd_tools_enable_batch(&names, &platform)?;
                }
                Some(ToolAction::Summary) => {
                    hermez_cli::config_cmd::cmd_tools_summary()?;
                }
                None => {
                    hermez_cli::tools_cmd::cmd_tools_list("cli")?;
                }
            }
        }
        Some(Commands::Skills { action }) => {
            match action {
                Some(SkillAction::List { source }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("list", None, None, &source, 20, 1, "", false)?;
                }
                Some(SkillAction::Search { query, source, limit }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("search", None, Some(&query), &source, limit, 1, "", false)?;
                }
                Some(SkillAction::Browse { page, size, source }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("browse", None, None, &source, size, page, "", false)?;
                }
                Some(SkillAction::Install { identifier, category, force, yes }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills_install(&identifier, &category, force, yes)?;
                }
                Some(SkillAction::Inspect { identifier }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("inspect", Some(&identifier), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Info { name }) => app.show_skill_info(&name)?,
                Some(SkillAction::Enable { name, platform }) => app.enable_skill(&name, platform.as_deref())?,
                Some(SkillAction::Disable { name, platform }) => app.disable_skill(&name, platform.as_deref())?,
                Some(SkillAction::Uninstall { name }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("uninstall", Some(&name), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Check { name }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("check", name.as_deref(), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Update { name }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("update", name.as_deref(), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Audit { name }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("audit", name.as_deref(), None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Commands) => app.list_skill_commands()?,
                Some(SkillAction::Publish { name, registry, repo }) => {
                    hermez_cli::skills_hub_cmd::cmd_skills_publish(&name, registry.as_deref(), repo.as_deref())?;
                }
                Some(SkillAction::Snapshot { snapshot_action }) => {
                    match snapshot_action {
                        Some(SnapshotAction::Export { output }) => {
                            hermez_cli::skills_hub_cmd::cmd_skills("snapshot-export", None, None, "all", 10, 1, output.as_deref().unwrap_or(""), false)?;
                        }
                        Some(SnapshotAction::Import { path, force }) => {
                            hermez_cli::skills_hub_cmd::cmd_skills("snapshot-import", Some(&path), None, "all", 10, 1, "", force)?;
                        }
                        None => {
                            hermez_cli::skills_hub_cmd::cmd_skills("snapshot-export", None, None, "all", 10, 1, "", false)?;
                        }
                    }
                }
                Some(SkillAction::Tap { tap_action }) => {
                    match tap_action {
                        Some(TapAction::List) => {
                            hermez_cli::skills_hub_cmd::cmd_skills("tap-list", None, None, "all", 10, 1, "", false)?;
                        }
                        Some(TapAction::Add { repo }) => {
                            hermez_cli::skills_hub_cmd::cmd_skills("tap-add", Some(&repo), None, "all", 10, 1, "", false)?;
                        }
                        Some(TapAction::Remove { name }) => {
                            hermez_cli::skills_hub_cmd::cmd_skills("tap-remove", Some(&name), None, "all", 10, 1, "", false)?;
                        }
                        None => {
                            hermez_cli::skills_hub_cmd::cmd_skills("tap-list", None, None, "all", 10, 1, "", false)?;
                        }
                    }
                }
                Some(SkillAction::Config) => {
                    hermez_cli::skills_hub_cmd::cmd_skills("config", None, None, "all", 10, 1, "", false)?;
                }
                Some(SkillAction::Reset) => {
                    hermez_cli::skills_hub_cmd::cmd_skills_reset()?;
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
                    hermez_cli::gateway_mgmt::cmd_gateway_start(all, system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Stop { all, system }) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_stop(all, system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Restart { system, all }) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_restart(system, all)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Status { deep, system }) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_status(deep, system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Install { force, system, run_as_user }) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_install(force, system, run_as_user.as_deref())
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Uninstall { system }) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_uninstall(system)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::Setup) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_setup()
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                Some(GatewayAction::MigrateLegacy) => {
                    hermez_cli::gateway_mgmt::cmd_gateway_migrate_legacy()
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
                    hermez_cli::profiles_cmd::cmd_profile_list()?;
                }
                Some(ProfileAction::Create { name, clone, clone_all, clone_from, no_alias }) => {
                    hermez_cli::profiles_cmd::cmd_profile_create(
                        &name, clone, clone_all, clone_from.as_deref(), no_alias,
                    )?;
                }
                Some(ProfileAction::Use { name }) => {
                    hermez_cli::profiles_cmd::cmd_profile_use(&name)?;
                }
                Some(ProfileAction::Delete { name, force, yes }) => {
                    hermez_cli::profiles_cmd::cmd_profile_delete(&name, force || yes)?;
                }
                Some(ProfileAction::Show { name }) => {
                    hermez_cli::profiles_cmd::cmd_profile_show(&name)?;
                }
                Some(ProfileAction::Alias { name, remove, alias_name }) => {
                    let target = alias_name.as_deref();
                    hermez_cli::profiles_cmd::cmd_profile_alias(&name, target, remove)?;
                }
                Some(ProfileAction::Rename { old_name, new_name }) => {
                    hermez_cli::profiles_cmd::cmd_profile_rename(&old_name, &new_name)?;
                }
                Some(ProfileAction::Export { name, output }) => {
                    hermez_cli::profiles_cmd::cmd_profile_export(&name, output.as_deref())?;
                }
                Some(ProfileAction::Import { path, name: profile_name }) => {
                    hermez_cli::profiles_cmd::cmd_profile_import(&path, profile_name.as_deref())?;
                }
                None => {
                    hermez_cli::profiles_cmd::cmd_profile_list()?;
                }
            }
        }
        Some(Commands::Sessions { action }) => {
            let db = hermez_state::SessionDB::open_default()?;
            match action {
                Some(SessionAction::List { limit, source }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_list(&db, limit, source.as_deref(), false)?;
                }
                Some(SessionAction::Delete { session_id, yes }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_delete(&db, &session_id, yes)?;
                }
                Some(SessionAction::Search { query, limit }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_search(&db, &query, limit)?;
                }
                Some(SessionAction::Stats { source }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_stats(&db, source.as_deref())?;
                }
                Some(SessionAction::Rename { session_id, title }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_rename(&db, &session_id, &title)?;
                }
                Some(SessionAction::Prune { older_than, source, yes }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_prune(&db, older_than, source.as_deref(), yes)?;
                }
                Some(SessionAction::Browse { source, limit }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_list(&db, limit, source.as_deref(), true)?;
                }
                Some(SessionAction::Export { path, source, session_id }) => {
                    hermez_cli::sessions_cmd::cmd_sessions_export(&db, &path, source.as_deref(), session_id.as_deref())?;
                }
                None => {
                    hermez_cli::sessions_cmd::cmd_sessions_list(&db, 20, None, false)?;
                }
            }
        }
        Some(Commands::Config { action }) => {
            match action {
                Some(ConfigAction::Show { verbose }) => {
                    hermez_cli::config_cmd::cmd_config_show(verbose)?;
                }
                Some(ConfigAction::Edit) => {
                    hermez_cli::config_cmd::cmd_config_edit()?;
                }
                Some(ConfigAction::Set { key, value }) => {
                    hermez_cli::config_cmd::cmd_config_set(&key, &value)?;
                }
                Some(ConfigAction::Path) => {
                    hermez_cli::config_cmd::cmd_config_path()?;
                }
                Some(ConfigAction::EnvPath) => {
                    hermez_cli::config_cmd::cmd_config_env_path()?;
                }
                Some(ConfigAction::Check) => {
                    hermez_cli::config_cmd::cmd_config_check()?;
                }
                Some(ConfigAction::Migrate) => {
                    hermez_cli::config_cmd::cmd_config_migrate()?;
                }
                None => {
                    hermez_cli::config_cmd::cmd_config_show(false)?;
                }
            }
        }
        Some(Commands::Batch { action }) => {
            match action {
                Some(BatchAction::Run { dataset, name, model, batch_size, workers, max_iterations, max_samples, resume, distribution }) => {
                    let opts = hermez_cli::batch_cmd::BatchRunOptions {
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
                    hermez_cli::batch_cmd::cmd_batch_run(&opts)?;
                }
                Some(BatchAction::Distributions) => {
                    hermez_cli::batch_cmd::cmd_batch_distributions()?;
                }
                Some(BatchAction::Status { name }) => {
                    hermez_cli::batch_cmd::cmd_batch_status(&name)?;
                }
                None => {
                    hermez_cli::batch_cmd::cmd_batch_distributions()?;
                }
            }
        }
        Some(Commands::Swe { action }) => {
            match action {
                Some(SweAction::Evaluate { dataset, split, sandbox, max_samples, output, model, quick, agent }) => {
                    let opts = hermez_cli::swe_cmd::SweEvaluateOptions {
                        dataset,
                        split,
                        sandbox,
                        max_samples,
                        output_dir: output,
                        model,
                        quick,
                        use_agent: agent,
                    };
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(hermez_cli::swe_cmd::cmd_swe_evaluate(&opts))?;
                }
                Some(SweAction::Benchmark { quick }) => {
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(hermez_cli::swe_cmd::cmd_swe_benchmark(quick))?;
                }
                Some(SweAction::Env) => {
                    hermez_cli::swe_cmd::cmd_swe_env_info()?;
                }
                None => {
                    hermez_cli::swe_cmd::cmd_swe_env_info()?;
                }
            }
        }
        Some(Commands::Cron { action }) => {
            match action {
                Some(CronAction::List { all }) => {
                    hermez_cli::cron_cmd::cmd_cron_list(all)?;
                }
                Some(CronAction::Create { name, schedule, command, prompt, delivery, paused, repeat, skill, script }) => {
                    hermez_cli::cron_cmd::cmd_cron_create(&name, &schedule, &command, prompt.as_deref(), &delivery.unwrap_or_else(|| "local".to_string()), !paused, repeat, skill.as_deref(), script.as_deref())?;
                }
                Some(CronAction::Delete { job_id, force }) => {
                    hermez_cli::cron_cmd::cmd_cron_delete(&job_id, force)?;
                }
                Some(CronAction::Pause { job_id }) => {
                    hermez_cli::cron_cmd::cmd_cron_pause(&job_id)?;
                }
                Some(CronAction::Resume { job_id }) => {
                    hermez_cli::cron_cmd::cmd_cron_resume(&job_id)?;
                }
                Some(CronAction::Edit { job_id, schedule, name, prompt, deliver, repeat, script, skill, add_skill, remove_skill, clear_skills }) => {
                    hermez_cli::cron_cmd::cmd_cron_edit(&job_id, schedule.as_deref(), name.as_deref(), prompt.as_deref(), deliver.as_deref(), repeat, script.as_deref(), skill.as_deref(), add_skill.as_deref(), remove_skill.as_deref(), clear_skills)?;
                }
                Some(CronAction::Run { job_id }) => {
                    hermez_cli::cron_cmd::cmd_cron_run(&job_id)?;
                }
                Some(CronAction::Status) => {
                    hermez_cli::cron_cmd::cmd_cron_status()?;
                }
                Some(CronAction::Tick) => {
                    hermez_cli::cron_cmd::cmd_cron_tick()?;
                }
                None => {
                    hermez_cli::cron_cmd::cmd_cron_list(false)?;
                }
            }
        }
        Some(Commands::Auth { action }) => {
            match action {
                Some(AuthAction::Add { provider, auth_type, api_key, label, client_id, no_browser, portal_url, inference_url, scope, timeout, insecure, ca_bundle }) => {
                    hermez_cli::auth_cmd::cmd_auth_add(
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
                    hermez_cli::auth_cmd::cmd_auth_list(provider.as_deref())?;
                }
                Some(AuthAction::Remove { provider, target }) => {
                    hermez_cli::auth_cmd::cmd_auth_remove(&provider, &target)?;
                }
                Some(AuthAction::Reset { provider }) => {
                    hermez_cli::auth_cmd::cmd_auth_reset(&provider)?;
                }
                Some(AuthAction::Status) => {
                    hermez_cli::auth_cmd::cmd_auth_status()?;
                }
                None => {
                    hermez_cli::auth_cmd::cmd_auth_status()?;
                }
            }
        }
        Some(Commands::Status { all, deep }) => {
            hermez_cli::status_cmd::cmd_status(all, deep)?;
        }
        Some(Commands::Insights { days, source }) => {
            hermez_cli::insights_cmd::cmd_insights(days, source.as_deref())?;
        }
        Some(Commands::Completion { shell }) => {
            hermez_cli::completion_cmd::cmd_completion(&shell)?;
        }
        Some(Commands::Version) => {
            hermez_cli::version_cmd::cmd_version();
        }
        Some(Commands::Logs { log_name, lines, follow, level, session, component, since }) => {
            hermez_cli::logs_cmd::cmd_logs(
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
                    hermez_cli::webhook_cmd::cmd_webhook_subscribe(
                        &name, &prompt, &events, &description, &deliver, deliver_chat_id, &skills, secret.as_deref(),
                    )?;
                }
                WebhookAction::List => {
                    hermez_cli::webhook_cmd::cmd_webhook_list()?;
                }
                WebhookAction::Remove { name } => {
                    hermez_cli::webhook_cmd::cmd_webhook_remove(&name)?;
                }
                WebhookAction::Test { name, payload } => {
                    hermez_cli::webhook_cmd::cmd_webhook_test(&name, &payload)?;
                }
            }
        }
        Some(Commands::Plugins { action }) => {
            match action {
                Some(PluginAction::Install { identifier, force }) => {
                    hermez_cli::plugins_cmd::cmd_plugins_install(&identifier, force)?;
                }
                Some(PluginAction::Update { name }) => {
                    hermez_cli::plugins_cmd::cmd_plugins_update(&name)?;
                }
                Some(PluginAction::Remove { name }) => {
                    hermez_cli::plugins_cmd::cmd_plugins_remove(&name)?;
                }
                Some(PluginAction::List) | None => {
                    hermez_cli::plugins_cmd::cmd_plugins_list()?;
                }
                Some(PluginAction::Enable { name }) => {
                    hermez_cli::plugins_cmd::cmd_plugins_enable(&name)?;
                }
                Some(PluginAction::Disable { name }) => {
                    hermez_cli::plugins_cmd::cmd_plugins_disable(&name)?;
                }
            }
        }
        Some(Commands::Memory { action }) => {
            match action {
                Some(MemoryAction::Setup) => {
                    hermez_cli::memory_cmd::cmd_memory_setup()?;
                }
                Some(MemoryAction::Status) => {
                    hermez_cli::memory_cmd::cmd_memory_status()?;
                }
                Some(MemoryAction::Off) => {
                    hermez_cli::memory_cmd::cmd_memory_off()?;
                }
                None => {
                    hermez_cli::memory_cmd::cmd_memory_status()?;
                }
            }
        }
        Some(Commands::Logout { provider }) => {
            hermez_cli::auth_cmd::cmd_logout(provider.as_deref())?;
        }
        Some(Commands::Import { path, force }) => {
            hermez_cli::backup_cmd::cmd_import(&path, force)?;
        }
        Some(Commands::Mcp { action }) => {
            match action {
                Some(McpAction::List) => {
                    hermez_cli::mcp_cmd::cmd_mcp_list()?;
                }
                Some(McpAction::Add { name, url, command, args, auth, preset, env }) => {
                    hermez_cli::mcp_cmd::cmd_mcp_add(&name, url.as_deref(), command.as_deref(), &args, auth.as_deref(), preset.as_deref(), &env)?;
                }
                Some(McpAction::Remove { name }) => {
                    hermez_cli::mcp_cmd::cmd_mcp("remove", Some(&name), "", &[])?;
                }
                Some(McpAction::Test { name }) => {
                    hermez_cli::mcp_cmd::cmd_mcp("test", Some(&name), "", &[])?;
                }
                Some(McpAction::Configure { name }) => {
                    hermez_cli::mcp_cmd::cmd_mcp_configure(&name)?;
                }
                Some(McpAction::Serve { verbose }) => {
                    hermez_cli::mcp_cmd::cmd_mcp_serve(verbose)?;
                }
                None => {
                    hermez_cli::mcp_cmd::cmd_mcp_list()?;
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
                    hermez_cli::model_cmd::cmd_model()?;
                }
                Some(ModelAction::Switch { model }) => {
                    hermez_cli::model_cmd::cmd_model_switch(&model)?;
                }
                Some(ModelAction::Info { model }) => {
                    hermez_cli::model_cmd::cmd_model_info(&model)?;
                }
            }
        }
        Some(Commands::Skin { action }) => {
            match action {
                Some(SkinAction::List) | None => {
                    hermez_cli::skin_engine::cmd_skin_list()?;
                }
                Some(SkinAction::Apply { name }) => {
                    hermez_cli::skin_engine::cmd_skin_apply(&name)?;
                }
                Some(SkinAction::Preview { name }) => {
                    hermez_cli::skin_engine::cmd_skin_preview(&name)?;
                }
            }
        }
        Some(Commands::Login { provider, client_id, no_browser, scopes, portal_url, inference_url, timeout, ca_bundle, insecure }) => {
            hermez_cli::login_cmd::cmd_login(&provider, client_id.as_deref(), no_browser, scopes.as_deref(), portal_url.as_deref(), inference_url.as_deref(), timeout, ca_bundle.as_deref(), insecure)?;
        }
        Some(Commands::Pairing { action }) => {
            match action {
                PairingAction::List => {
                    hermez_cli::pairing_cmd::cmd_pairing_list()?;
                }
                PairingAction::Approve { platform, code } => {
                    hermez_cli::pairing_cmd::cmd_pairing_approve(&platform, &code)?;
                }
                PairingAction::Revoke { platform, code } => {
                    hermez_cli::pairing_cmd::cmd_pairing_revoke(&platform, &code)?;
                }
                PairingAction::ClearPending => {
                    hermez_cli::pairing_cmd::cmd_pairing_clear_pending()?;
                }
            }
        }
        Some(Commands::Update { preview, force, gateway }) => {
            hermez_cli::update_cmd::cmd_update(preview, force, gateway)?;
        }
        Some(Commands::Uninstall { keep_data, keep_config, yes }) => {
            hermez_cli::uninstall_cmd::cmd_uninstall(keep_data, keep_config, yes)?;
        }
        Some(Commands::Dashboard { port, host, no_open, insecure, serve }) => {
            if serve {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(hermez_cli::web_server::run_server(&host, port))?;
            } else {
                hermez_cli::dashboard_cmd::cmd_dashboard_with_opts(&host, port, no_open, insecure)?;
            }
        }
        Some(Commands::WhatsApp { action, token, phone_id }) => {
            hermez_cli::whatsapp_cmd::cmd_whatsapp(&action, token.as_deref(), phone_id.as_deref())?;
        }
        Some(Commands::Acp { action, editor }) => {
            hermez_cli::acp_cmd::cmd_acp(action.as_deref().unwrap_or("status"), editor.as_deref())?;
        }
        Some(Commands::Claw { action, source, force, dry_run, preset, overwrite, migrate_secrets, yes, workspace_target, skill_conflict }) => {
            hermez_cli::claw_cmd::cmd_claw(&action, source.as_deref(), force, dry_run, &preset, overwrite, migrate_secrets, yes, workspace_target.as_deref(), &skill_conflict)?;
        }
    }

    Ok(())
}
