#[cfg(target_os = "macos")]
use crate::AppHandleManager;
use crate::{
    config::{Config, PrfItem, IVerge},
    core::*,
    feat,
    ipc::IpcManager,
    logging, logging_error,
    module::lightweight::{self, auto_lightweight_mode_init},
    process::AsyncHandler,
    utils::{init, logging::Type, server},
    wrap_err,
};
use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use parking_lot::{Mutex, RwLock};
use percent_encoding::percent_decode_str;
use scopeguard;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

use tauri::Url;
//#[cfg(not(target_os = "linux"))]
// use window_shadows::set_shadow;

pub static VERSION: OnceCell<String> = OnceCell::new();

// 定义默认窗口尺寸常量
const DEFAULT_WIDTH: u32 = 940;
const DEFAULT_HEIGHT: u32 = 700;

// 添加全局UI准备就绪标志
static UI_READY: OnceCell<RwLock<bool>> = OnceCell::new();

// 窗口创建锁，防止并发创建窗口
static WINDOW_CREATING: OnceCell<Mutex<(bool, Instant)>> = OnceCell::new();

// UI就绪阶段状态枚举
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiReadyStage {
    NotStarted,
    Loading,
    DomReady,
    ResourcesLoaded,
    Ready,
}

// UI就绪详细状态
#[derive(Debug)]
struct UiReadyState {
    stage: RwLock<UiReadyStage>,
}

impl Default for UiReadyState {
    fn default() -> Self {
        Self {
            stage: RwLock::new(UiReadyStage::NotStarted),
        }
    }
}

// 获取UI就绪状态细节
static UI_READY_STATE: OnceCell<UiReadyState> = OnceCell::new();

fn get_window_creating_lock() -> &'static Mutex<(bool, Instant)> {
    WINDOW_CREATING.get_or_init(|| Mutex::new((false, Instant::now())))
}

fn get_ui_ready() -> &'static RwLock<bool> {
    UI_READY.get_or_init(|| RwLock::new(false))
}

fn get_ui_ready_state() -> &'static UiReadyState {
    UI_READY_STATE.get_or_init(UiReadyState::default)
}

// 更新UI准备阶段
pub fn update_ui_ready_stage(stage: UiReadyStage) {
    let state = get_ui_ready_state();
    let mut stage_lock = state.stage.write();

    *stage_lock = stage;
    // 如果是最终阶段，标记UI完全就绪
    if stage == UiReadyStage::Ready {
        mark_ui_ready();
    }
}

// 标记UI已准备就绪
pub fn mark_ui_ready() {
    let mut ready = get_ui_ready().write();
    *ready = true;
    logging!(info, Type::Window, true, "UI已标记为完全就绪");
}

// 重置UI就绪状态
pub fn reset_ui_ready() {
    {
        let mut ready = get_ui_ready().write();
        *ready = false;
    }
    {
        let state = get_ui_ready_state();
        let mut stage = state.stage.write();
        *stage = UiReadyStage::NotStarted;
    }
    logging!(info, Type::Window, true, "UI就绪状态已重置");
}

/// 异步方式处理启动后的额外任务
pub async fn resolve_setup_async(app_handle: &AppHandle) {
    let start_time = std::time::Instant::now();
    logging!(
        info,
        Type::Setup,
        true,
        "开始执行异步设置任务... 线程ID: {:?}",
        std::thread::current().id()
    );

    if VERSION.get().is_none() {
        let version = app_handle.package_info().version.to_string();
        VERSION.get_or_init(|| {
            logging!(info, Type::Setup, true, "初始化版本信息: {}", version);
            version.clone()
        });
    }

    logging_error!(Type::Setup, true, init::init_scheme());

    logging_error!(Type::Setup, true, init::startup_script().await);

    logging!(
        info,
        Type::Config,
        true,
        "开始初始化配置... 线程ID: {:?}",
        std::thread::current().id()
    );
    logging_error!(Type::Config, true, Config::init_config().await);
    logging!(info, Type::Config, true, "配置初始化完成");

    // 启动时清理冗余的 Profile 文件
    logging!(info, Type::Setup, true, "开始清理冗余的Profile文件...");

    match Config::profiles().latest_ref().auto_cleanup() {
        Ok(_) => {
            logging!(info, Type::Setup, true, "启动时Profile文件清理完成");
        }
        Err(e) => {
            logging!(warn, Type::Setup, true, "启动时清理Profile文件失败: {}", e);
        }
    }

    logging!(trace, Type::Core, true, "启动核心管理器...");
    logging_error!(Type::Core, true, CoreManager::global().init().await);

    log::trace!(target: "app", "启动内嵌服务器...");
    server::embed_server();

    logging!(trace, Type::Core, true, "启动 IPC 监控服务...");

    logging_error!(Type::Tray, true, tray::Tray::global().init());

    if let Some(app_handle) = handle::Handle::global().app_handle() {
        logging!(info, Type::Tray, true, "创建系统托盘...");
        let result = tray::Tray::global().create_tray_from_handle(&app_handle);
        if result.is_ok() {
            logging!(info, Type::Tray, true, "系统托盘创建成功");
        } else if let Err(e) = result {
            logging!(error, Type::Tray, true, "系统托盘创建失败: {}", e);
        }
    } else {
        logging!(
            error,
            Type::Tray,
            true,
            "无法创建系统托盘: app_handle不存在"
        );
    }

    // 更新系统代理
    logging_error!(
        Type::System,
        true,
        sysopt::Sysopt::global().update_sysproxy().await
    );
    logging_error!(
        Type::System,
        true,
        sysopt::Sysopt::global().init_guard_sysproxy()
    );

    // 创建窗口
    let is_silent_start = { Config::verge().latest_ref().enable_silent_start }.unwrap_or(false);
    #[cfg(target_os = "macos")]
    {
        if is_silent_start {
            use crate::AppHandleManager;

            AppHandleManager::global().set_activation_policy_accessory();
        }
    }
    create_window(!is_silent_start);

    // 初始化定时器
    logging_error!(Type::System, true, timer::Timer::global().init());

    // 自动进入轻量模式
    auto_lightweight_mode_init();

    logging_error!(Type::Tray, true, tray::Tray::global().update_part());

    logging!(trace, Type::System, true, "初始化热键...");
    logging_error!(Type::System, true, hotkey::Hotkey::global().init());

    // 自动启用跟随系统启动功能
    if let Err(e) = auto_enable_autostart_on_system_startup().await {
        logging!(
            warn,
            Type::Setup,
            true,
            "自动启用跟随系统启动功能失败: {}",
            e
        );
    }

    // 启动时自动导入订阅URL（Windows系统首次执行完会重启应用）
    auto_import_startup_urls().await;

    let elapsed = start_time.elapsed();
    logging!(
        info,
        Type::Setup,
        true,
        "异步设置任务完成，耗时: {:?}",
        elapsed
    );

    // 如果初始化时间过长，记录警告
    if elapsed.as_secs() > 10 {
        logging!(
            warn,
            Type::Setup,
            true,
            "异步设置任务耗时较长({:?})",
            elapsed
        );
    }
}

/// reset system proxy (异步)
pub async fn resolve_reset_async() {
    #[cfg(target_os = "macos")]
    logging!(info, Type::Tray, true, "Unsubscribing from traffic updates");
    #[cfg(target_os = "macos")]
    tray::Tray::global().unsubscribe_traffic();

    logging_error!(
        Type::System,
        true,
        sysopt::Sysopt::global().reset_sysproxy().await
    );
    logging_error!(Type::Core, true, CoreManager::global().stop_core().await);
    #[cfg(target_os = "macos")]
    {
        logging!(info, Type::System, true, "Restoring system DNS settings");
        restore_public_dns().await;
    }
}

/// Create the main window
pub fn create_window(is_show: bool) -> bool {
    logging!(
        info,
        Type::Window,
        true,
        "开始创建/显示主窗口, is_show={}",
        is_show
    );

    if !is_show {
        logging!(info, Type::Window, true, "静默模式启动时不创建窗口");
        lightweight::set_lightweight_mode(true);
        handle::Handle::notify_startup_completed();
        return false;
    }

    if let Some(app_handle) = handle::Handle::global().app_handle() {
        if let Some(window) = app_handle.get_webview_window("main") {
            logging!(info, Type::Window, true, "主窗口已存在，将显示现有窗口");
            if is_show {
                if window.is_minimized().unwrap_or(false) {
                    logging!(info, Type::Window, true, "窗口已最小化，正在取消最小化");
                    let _ = window.unminimize();
                }
                let _ = window.show();
                let _ = window.set_focus();

                #[cfg(target_os = "macos")]
                {
                    AppHandleManager::global().set_activation_policy_regular();
                }
            }
            return true;
        }
    }

    let creating_lock = get_window_creating_lock();
    let mut creating = creating_lock.lock();

    let (is_creating, last_time) = *creating;
    let elapsed = last_time.elapsed();

    if is_creating && elapsed < Duration::from_secs(2) {
        logging!(
            info,
            Type::Window,
            true,
            "窗口创建请求被忽略，因为最近创建过 ({:?}ms)",
            elapsed.as_millis()
        );
        return false;
    }

    *creating = (true, Instant::now());

    // ScopeGuard 确保创建状态重置，防止 webview 卡死
    let _guard = scopeguard::guard(creating, |mut creating_guard| {
        *creating_guard = (false, Instant::now());
        logging!(debug, Type::Window, true, "[ScopeGuard] 窗口创建状态已重置");
    });

    let app_handle = match handle::Handle::global().app_handle() {
        Some(handle) => handle,
        None => {
            logging!(
                error,
                Type::Window,
                true,
                "无法获取app_handle，窗口创建失败"
            );
            return false;
        }
    };

    match tauri::WebviewWindowBuilder::new(
        &app_handle,
        "main", /* the unique window label */
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("Clash Verge")
    .center()
    .decorations(true)
    .fullscreen(false)
    .inner_size(DEFAULT_WIDTH as f64, DEFAULT_HEIGHT as f64)
    .min_inner_size(520.0, 520.0)
    .visible(true) // 立即显示窗口，避免用户等待
    .initialization_script(
        r#"
        console.log('[Tauri] 窗口初始化脚本开始执行');

        function createLoadingOverlay() {

            if (document.getElementById('initial-loading-overlay')) {
                console.log('[Tauri] 加载指示器已存在');
                return;
            }

            console.log('[Tauri] 创建加载指示器');
            const loadingDiv = document.createElement('div');
            loadingDiv.id = 'initial-loading-overlay';
            loadingDiv.innerHTML = `
                <div style="
                    position: fixed; top: 0; left: 0; right: 0; bottom: 0;
                    background: var(--bg-color, #f5f5f5); color: var(--text-color, #333);
                    display: flex; flex-direction: column; align-items: center;
                    justify-content: center; z-index: 9999;
                    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
                    transition: opacity 0.3s ease;
                ">
                    <div style="margin-bottom: 20px;">
                        <div style="
                            width: 40px; height: 40px; border: 3px solid #e3e3e3;
                            border-top: 3px solid #3498db; border-radius: 50%;
                            animation: spin 1s linear infinite;
                        "></div>
                    </div>
                    <div style="font-size: 14px; opacity: 0.7;">Loading Clash Verge...</div>
                </div>
                <style>
                    @keyframes spin {
                        0% { transform: rotate(0deg); }
                        100% { transform: rotate(360deg); }
                    }
                    @media (prefers-color-scheme: dark) {
                        :root { --bg-color: #1a1a1a; --text-color: #ffffff; }
                    }
                </style>
            `;

            if (document.body) {
                document.body.appendChild(loadingDiv);
            } else {
                document.addEventListener('DOMContentLoaded', () => {
                    if (document.body && !document.getElementById('initial-loading-overlay')) {
                        document.body.appendChild(loadingDiv);
                    }
                });
            }
        }

        createLoadingOverlay();

        if (document.readyState === 'loading') {
            document.addEventListener('DOMContentLoaded', createLoadingOverlay);
        } else {
            createLoadingOverlay();
        }

        console.log('[Tauri] 窗口初始化脚本执行完成');
    "#,
    )
    .build()
    {
        Ok(newly_created_window) => {
            logging!(debug, Type::Window, true, "主窗口实例创建成功");

            update_ui_ready_stage(UiReadyStage::NotStarted);

            AsyncHandler::spawn(move || async move {
                handle::Handle::global().mark_startup_completed();
                logging!(
                    debug,
                    Type::Window,
                    true,
                    "异步窗口任务开始 (启动已标记完成)"
                );

                // 先运行轻量模式检测
                lightweight::run_once_auto_lightweight();

                // 发送启动完成事件，触发前端开始加载
                logging!(
                    debug,
                    Type::Window,
                    true,
                    "发送 verge://startup-completed 事件"
                );
                handle::Handle::notify_startup_completed();

                if is_show {
                    let window_clone = newly_created_window.clone();

                    // 立即显示窗口
                    let _ = window_clone.show();
                    let _ = window_clone.set_focus();
                    logging!(info, Type::Window, true, "窗口已立即显示");
                    #[cfg(target_os = "macos")]
                    {
                        AppHandleManager::global().set_activation_policy_regular();
                    }

                    let timeout_seconds = if crate::module::lightweight::is_in_lightweight_mode() {
                        3
                    } else {
                        8
                    };

                    logging!(
                        info,
                        Type::Window,
                        true,
                        "开始监控UI加载状态 (最多{}秒)...",
                        timeout_seconds
                    );

                    // 异步监控UI状态，使用try_read避免死锁
                    AsyncHandler::spawn(move || async move {
                        logging!(
                            debug,
                            Type::Window,
                            true,
                            "启动UI状态监控线程，超时{}秒",
                            timeout_seconds
                        );

                        let ui_ready_checker = || async {
                            let (mut check_count, mut consecutive_failures) = (0, 0);

                            loop {
                                let is_ready = get_ui_ready()
                                    .try_read()
                                    .map(|guard| *guard)
                                    .unwrap_or_else(|| {
                                        consecutive_failures += 1;
                                        if consecutive_failures > 50 {
                                            logging!(
                                                warn,
                                                Type::Window,
                                                true,
                                                "UI状态监控连续{}次无法获取读锁，可能存在死锁",
                                                consecutive_failures
                                            );
                                            consecutive_failures = 0;
                                        }
                                        false
                                    });

                                if is_ready {
                                    logging!(
                                        debug,
                                        Type::Window,
                                        true,
                                        "UI状态监控检测到就绪信号，退出监控"
                                    );
                                    return;
                                }

                                consecutive_failures = 0;
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                check_count += 1;

                                if check_count % 20 == 0 {
                                    logging!(
                                        debug,
                                        Type::Window,
                                        true,
                                        "UI加载状态检查... ({}秒)",
                                        check_count / 10
                                    );
                                }
                            }
                        };

                        let wait_result = tokio::time::timeout(
                            Duration::from_secs(timeout_seconds),
                            ui_ready_checker(),
                        )
                        .await;

                        match wait_result {
                            Ok(_) => {
                                logging!(info, Type::Window, true, "UI已完全加载就绪");
                                handle::Handle::global()
                                    .get_window()
                                    .map(|window| window.eval(r"
                                        const overlay = document.getElementById('initial-loading-overlay');
                                        if (overlay) {
                                            overlay.style.opacity = '0';
                                            setTimeout(() => overlay.remove(), 300);
                                        }
                                    "));
                            }
                            Err(_) => {
                                logging!(
                                    warn,
                                    Type::Window,
                                    true,
                                    "UI加载监控超时({}秒)，但窗口已正常显示",
                                    timeout_seconds
                                );

                                get_ui_ready()
                                    .try_write()
                                    .map(|mut guard| {
                                        *guard = true;
                                        logging!(
                                            info,
                                            Type::Window,
                                            true,
                                            "超时后成功设置UI就绪状态"
                                        );
                                    })
                                    .unwrap_or_else(|| {
                                        logging!(
                                            error,
                                            Type::Window,
                                            true,
                                            "超时后无法获取UI状态写锁，可能存在严重死锁"
                                        );
                                    });
                            }
                        }
                    });

                    logging!(info, Type::Window, true, "窗口显示流程完成");
                } else {
                    logging!(
                        debug,
                        Type::Window,
                        true,
                        "is_show为false，窗口保持隐藏状态"
                    );
                }
            });
            true
        }
        Err(e) => {
            logging!(error, Type::Window, true, "主窗口构建失败: {}", e);
            false
        }
    }
}

pub async fn resolve_scheme(param: String) -> Result<()> {
    log::info!(target:"app", "received deep link: {param}");

    let param_str = if param.starts_with("[") && param.len() > 4 {
        param
            .get(2..param.len() - 2)
            .ok_or_else(|| anyhow::anyhow!("Invalid string slice boundaries"))?
    } else {
        param.as_str()
    };

    // 解析 URL
    let link_parsed = match Url::parse(param_str) {
        Ok(url) => url,
        Err(e) => {
            bail!("failed to parse deep link: {:?}, param: {:?}", e, param);
        }
    };

    if link_parsed.scheme() == "clash" || link_parsed.scheme() == "clash-verge" {
        // 检查系统代理参数
        let enable_system_proxy_param = link_parsed
            .query_pairs()
            .find(|(key, _)| key == "enable_system_proxy")
            .map(|(_, value)| value.into_owned());
        if let Some(ref proxy_value) = enable_system_proxy_param {
            log::info!(target:"app", "processing system proxy control: {}", proxy_value);
            
            let enable_proxy = match proxy_value.to_lowercase().as_str() {
                "true" | "1" | "on" | "enable" => true,
                "false" | "0" | "off" | "disable" => false,
                _ => {
                    logging!(error, Type::Config, true, "Invalid enable_system_proxy value: {}. Use 'true' or 'false'", proxy_value);
                    false
                }
            };

            // 获取当前系统代理状态
            let current_enabled = {
                let verge = Config::verge();
                let verge_config = verge.latest_ref();
                verge_config.enable_system_proxy.unwrap_or(false)
            };

            // 如果状态不同，则更新系统代理设置
            if current_enabled != enable_proxy {
                match feat::patch_verge(
                    IVerge {
                        enable_system_proxy: Some(enable_proxy),
                        ..IVerge::default()
                    },
                    false,
                ).await {
                    Ok(_) => {
                        let action = if enable_proxy { "启用" } else { "禁用" };
                        logging!(info, Type::Config, true, "通过URI协议{}系统代理成功", action);
                        refresh_ui_after_config_change(false, false, false, true, 100);
                    }
                    Err(e) => {
                        logging!(error, Type::Config, true, "通过URI协议控制系统代理失败: {}", e);
                    }
                }
            } else {
                let status = if enable_proxy { "已启用" } else { "已禁用" };
                logging!(info, Type::Config, true, "系统代理状态无变化，当前{}", status);
            }
        }
        
        let name = link_parsed
            .query_pairs()
            .find(|(key, _)| key == "name")
            .map(|(_, value)| value.into_owned());

        let url_param = if let Some(query) = link_parsed.query() {
            let prefix = "url=";
            if let Some(pos) = query.find(prefix) {
                let raw_url = &query[pos + prefix.len()..];
                Some(percent_decode_str(raw_url).decode_utf8_lossy().to_string())
            } else {
                None
            }
        } else {
            None
        };

        // 处理订阅导入（原有逻辑）
        match url_param {
            Some(url) => {
                log::info!(target:"app", "decoded subscription url: {url}");

                create_window(false);
                match PrfItem::from_url(url.as_ref(), name, None, None).await {
                    Ok(item) => {
                        let uid = match item.uid.clone() {
                            Some(uid) => uid,
                            None => {
                                logging!(error, Type::Config, true, "Profile item missing UID");
                                handle::Handle::notice_message(
                                    "import_sub_url::error",
                                    "Profile item missing UID".to_string(),
                                );
                                return Ok(());
                            }
                        };
                        let _ = wrap_err!(Config::profiles().data_mut().append_item(item));
                        handle::Handle::notice_message("import_sub_url::ok", uid);
                    }
                    Err(e) => {
                        handle::Handle::notice_message("import_sub_url::error", e.to_string());
                    }
                }
            }
            None => {
                logging!(error, Type::Config, true, "failed to get profile url");
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn set_public_dns(dns_server: String) {
    use crate::{core::handle, utils::dirs};
    use tauri_plugin_shell::ShellExt;
    let app_handle = match handle::Handle::global().app_handle() {
        Some(handle) => handle,
        None => {
            log::error!(target: "app", "app_handle not available for DNS configuration");
            return;
        }
    };

    log::info!(target: "app", "try to set system dns");
    let resource_dir = match dirs::app_resources_dir() {
        Ok(dir) => dir,
        Err(e) => {
            log::error!(target: "app", "Failed to get resource directory: {}", e);
            return;
        }
    };
    let script = resource_dir.join("set_dns.sh");
    if !script.exists() {
        log::error!(target: "app", "set_dns.sh not found");
        return;
    }
    let script = script.to_string_lossy().into_owned();
    match app_handle
        .shell()
        .command("bash")
        .args([script, dns_server])
        .current_dir(resource_dir)
        .status()
        .await
    {
        Ok(status) => {
            if status.success() {
                log::info!(target: "app", "set system dns successfully");
            } else {
                let code = status.code().unwrap_or(-1);
                log::error!(target: "app", "set system dns failed: {code}");
            }
        }
        Err(err) => {
            log::error!(target: "app", "set system dns failed: {err}");
        }
    }
}

#[cfg(target_os = "macos")]
pub async fn restore_public_dns() {
    use crate::{core::handle, utils::dirs};
    use tauri_plugin_shell::ShellExt;
    let app_handle = match handle::Handle::global().app_handle() {
        Some(handle) => handle,
        None => {
            log::error!(target: "app", "app_handle not available for DNS restoration");
            return;
        }
    };
    log::info!(target: "app", "try to unset system dns");
    let resource_dir = match dirs::app_resources_dir() {
        Ok(dir) => dir,
        Err(e) => {
            log::error!(target: "app", "Failed to get resource directory: {}", e);
            return;
        }
    };
    let script = resource_dir.join("unset_dns.sh");
    if !script.exists() {
        log::error!(target: "app", "unset_dns.sh not found");
        return;
    }
    let script = script.to_string_lossy().into_owned();
    match app_handle
        .shell()
        .command("bash")
        .args([script])
        .current_dir(resource_dir)
        .status()
        .await
    {
        Ok(status) => {
            if status.success() {
                log::info!(target: "app", "unset system dns successfully");
            } else {
                let code = status.code().unwrap_or(-1);
                log::error!(target: "app", "unset system dns failed: {code}");
            }
        }
        Err(err) => {
            log::error!(target: "app", "unset system dns failed: {err}");
        }
    }
}

/// 启动时自动导入订阅URL
/// 
/// ## 配置说明
/// 
/// 在 `verge.yaml` 配置文件中添加以下配置：
/// 
/// ```yaml
/// # 启用启动时自动导入订阅功能
/// enable_startup_import: true
/// 
/// # 要自动导入的订阅URL列表
/// startup_import_urls:
///   - "https://example.com/subscription1"
///   - "https://example.com/subscription2"
/// ```
/// 
/// ## 功能特性
/// 
/// - 支持多个订阅URL同时导入
/// - 自动跳过空白或无效的URL
/// - 提供详细的日志记录
/// - 导入成功/失败时会发送通知消息
/// - 不会阻塞应用启动流程
/// - 支持导入完成后自动切换到最新配置
/// 
/// ## 自动切换逻辑
/// 
/// - 只有在至少一个配置成功导入时才会执行切换
/// - 切换到最后一个成功导入的配置
/// - 切换过程中会发送相应的通知消息
/// 
/// ## 使用场景
/// 
/// - 企业环境下的统一配置分发
/// - 新设备首次启动时的自动配置
/// - 定期更新的订阅源自动导入
/// - 自动激活最新的代理配置
/// 
pub async fn auto_import_startup_urls() {
    // 提取所需的配置值，避免跨await持有锁
    let (enable_startup_import, urls) = {
        let verge = Config::verge();
        let verge_config = verge.latest_ref();
        
        let enable_startup_import = verge_config.enable_startup_import.unwrap_or(false);
        let urls = verge_config.startup_import_urls.clone().unwrap_or_default();
        
        (enable_startup_import, urls)
    };
    
    #[cfg(target_os = "windows")]
    let is_first_startup = {
        let verge = Config::verge();
        let verge_config = verge.latest_ref();
        verge_config.is_first_startup.unwrap_or(true)
    };
    
    // 检查是否启用了启动时自动导入
    if !enable_startup_import {
        logging!(debug, Type::Config, true, "启动时自动导入功能未启用");
        return;
    }

    // 检查URL列表
    if urls.is_empty() {
        logging!(debug, Type::Config, true, "没有配置启动时自动导入的URL");
        return;
    }

    logging!(info, Type::Config, true, "开始启动时自动导入订阅，共{}个URL", urls.len());

    let mut success_count = 0;
    let mut failed_count = 0;
    let mut last_successful_uid: Option<String> = None;

    for (index, url) in urls.iter().enumerate() {
        if url.trim().is_empty() {
            continue;
        }

        logging!(info, Type::Config, true, "正在导入第{}个订阅: {}", index + 1, url);
        
        // 尝试导入，单个url最多重试2次
        let mut retry_count = 0;
        let max_retries = 2;
        
        loop {
            match import_subscription_from_url(url.clone(), None).await {
                Ok(uid) => {
                    logging!(info, Type::Config, true, "成功导入订阅: {} (UID: {})", url, uid);
                    success_count += 1;
                    last_successful_uid = Some(uid);
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count <= max_retries {
                        logging!(warn, Type::Config, true, "导入订阅失败，第{}次重试: {} - {}", retry_count, url, e);
                        // 等待1秒后重试
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    } else {
                        logging!(error, Type::Config, true, "导入订阅最终失败: {} - {}", url, e);
                        failed_count += 1;
                        break;
                    }
                }
            }
        }
    }

    if success_count > 0 || failed_count > 0 {
        let summary = format!("启动导入完成: 成功{}个, 失败{}个", success_count, failed_count);
        logging!(info, Type::Config, true, "{}", summary);
    }

    // 处理导入成功后的逻辑
    if last_successful_uid.is_some() {
        if let Some(uid) = last_successful_uid {
            logging!(info, Type::Config, true, "准备处理最新导入的配置: {}", uid);
            
            // Windows端首次启动特殊处理
            #[cfg(target_os = "windows")]
            if is_first_startup {
                logging!(info, Type::Config, true, "Windows端首次启动，导入配置完成后将重启应用");
                
                // 更新首次启动标记
                let verge_patch = crate::config::IVerge {
                    is_first_startup: Some(false),
                    ..Default::default()
                };
                Config::verge().draft_mut().patch_config(verge_patch);
                Config::verge().apply();
                let _ = Config::verge().data_mut().save_file();
                logging!(info, Type::Config, true, "已标记为非首次启动");
                
                // 延迟重启应用
                AsyncHandler::spawn(move || async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    logging!(info, Type::Config, true, "Windows端首次启动配置导入完成，即将重启应用");
                    
                    if let Some(app_handle) = handle::Handle::global().app_handle() {
                        logging!(info, Type::Config, true, "获取到应用句柄，即将重启应用");
                        tauri::process::restart(&app_handle.env());
                    } else {
                        logging!(error, Type::Config, true, "无法获取应用句柄，重启失败");
                    }
                });
                
                return;
            }
                
            // 延迟执行配置切换，确保核心已完全启动
            let switch_uid = uid.clone();
            AsyncHandler::spawn(move || async move {
                logging!(info, Type::Config, true, "启动异步配置切换任务: {}", switch_uid);

                // 记录初始状态
                let initial_core_mode = CoreManager::global().get_running_mode();
                logging!(info, Type::Config, true, "异步任务开始时核心状态: {:?}", initial_core_mode);
                
                // 等待核心启动完成
                wait_for_core_ready().await;
                
                logging!(info, Type::Config, true, "核心已就绪，开始切换到配置: {}", switch_uid);
                
                match switch_to_profile_with_retry(switch_uid.clone(), 3).await {
                    Ok(_) => {
                        logging!(info, Type::Config, true, "成功切换到配置: {}", switch_uid);
                        
                        // 自动开启系统代理
                        if let Err(e) = auto_enable_system_proxy_after_import().await {
                            logging!(warn, Type::Config, true, "自动开启系统代理失败: {}", e);
                        }
                        // 使用统一的UI刷新函数
                        logging!(info, Type::Config, true, "准备刷新配置切换后的界面");
                        // 配置切换后刷新：Clash界面 + Verge界面 + 托盘菜单，延迟100ms
                        refresh_ui_after_config_change(
                            true,  // refresh_clash: 刷新代理列表
                            true,  // refresh_verge: 刷新配置状态
                            true,  // update_tray_menu: 更新托盘菜单
                            true, // update_tray_icon: 更新图标
                            100    // delay_ms: 100ms延迟确保配置应用
                        );
                    }
                    Err(e) => {
                        logging!(error, Type::Config, true, "切换到配置失败: {} - {}", switch_uid, e);
                    }
                }
            });
        }
    }

    logging!(info, Type::Config, true, "启动时自动导入订阅完成");
}

/// 从URL导入订阅配置
/// 复用resolve_scheme中的逻辑，但简化为直接处理URL
async fn import_subscription_from_url(url: String, name: Option<String>) -> Result<String> {
    logging!(info, Type::Config, true, "开始从URL导入订阅: {}", url);

    // 检查是否已存在相同URL的订阅
    let existing_uid = {
        let profiles_config = Config::profiles();
        let profiles = profiles_config.latest_ref();
        if let Some(items) = profiles.get_items() {
            for item in items {
                if let Some(existing_url) = &item.url {
                    if existing_url == &url {
                        logging!(info, Type::Config, true, "订阅URL已存在，跳过导入: {}", url);
                        if let Some(uid) = &item.uid {
                            return Ok(uid.clone());
                        }
                    }
                }
            }
        }
        None::<String>
    };
    
    // 如果找到了现有的UID，直接返回
    if let Some(uid) = existing_uid {
        return Ok(uid);
    }

    // 使用PrfItem::from_url创建配置项
    match PrfItem::from_url(url.as_ref(), name, None, None).await {
        Ok(item) => {
            let uid = match item.uid.clone() {
                Some(uid) => uid,
                None => {
                    logging!(error, Type::Config, true, "配置项缺少UID");
                    bail!("Profile item missing UID");
                }
            };
            // 添加到配置中
            let _ = wrap_err!(Config::profiles().data_mut().append_item(item));
            logging!(info, Type::Config, true, "成功导入订阅配置，UID: {}", uid);
            Ok(uid)
        }
        Err(e) => {
            logging!(error, Type::Config, true, "从URL创建配置项失败: {}", e);
            Err(e)
        }
    }
}

/// 带重试机制的配置切换函数
/// 确保在核心启动完成后能成功切换配置
async fn switch_to_profile_with_retry(uid: String, max_retries: u32) -> Result<()> {
    let mut attempts = 0;
    let mut last_error = None;
    
    logging!(info, Type::Config, true, "开始带重试的配置切换: {}, 最大重试次数: {}", uid, max_retries);
    
    while attempts < max_retries {
        attempts += 1;
        
        logging!(info, Type::Config, true, "尝试切换配置 (第{}次): {}", attempts, uid);
        
        // 在每次尝试前检查核心状态
        let core_mode = CoreManager::global().get_running_mode();
        logging!(info, Type::Config, true, "重试前核心状态: {:?}", core_mode);
        
        match switch_to_profile(uid.clone()).await {
            Ok(_) => {
                logging!(info, Type::Config, true, "配置切换成功 (第{}次尝试): {}", attempts, uid);
                
                // 验证切换结果
                {
                    let profiles_config = Config::profiles();
                    let profiles = profiles_config.latest_ref();
                    let current = profiles.get_current();
                    logging!(info, Type::Config, true, "重试成功后验证当前配置: {:?}", current);
                }
                
                return Ok(());
            }
            Err(e) => {
                last_error = Some(e);
                logging!(warn, Type::Config, true, "配置切换失败 (第{}次尝试): {} - {}", attempts, uid, last_error.as_ref().unwrap());
                
                if attempts < max_retries {
                    // 等待一段时间后重试
                    let delay = std::time::Duration::from_secs(1 * attempts as u64);
                    logging!(info, Type::Config, true, "等待{}秒后重试...", delay.as_secs());
                    tokio::time::sleep(delay).await;
                } else {
                    logging!(error, Type::Config, true, "已达到最大重试次数: {}", max_retries);
                }
            }
        }
    }
    
    // 所有重试都失败了
    let final_error = last_error.unwrap_or_else(|| anyhow::anyhow!("未知错误"));
    logging!(error, Type::Config, true, "配置切换最终失败，已重试{}次: {} - {}", max_retries, uid, final_error);
    Err(final_error)
}

/// 切换到指定的配置文件
/// 复用现有的配置切换逻辑
async fn switch_to_profile(uid: String) -> Result<()> {
    use crate::config::IProfiles;
    use crate::cmd::profile::patch_profiles_config;
    
    logging!(info, Type::Config, true, "开始切换到配置: {}", uid);
    
    // 检查配置是否存在
    {
        let profiles_config = Config::profiles();
        let profiles = profiles_config.latest_ref();
        match profiles.get_item(&uid) {
            Ok(item) => {
                logging!(info, Type::Config, true, "找到目标配置: name={:?}, file={:?}", item.name, item.file);
            }
            Err(e) => {
                logging!(error, Type::Config, true, "配置不存在: {} - {}", uid, e);
                return Err(e);
            }
        }
    }
    
    // 检查当前配置状态，避免重复切换
    let current_uid = {
        let profiles_config = Config::profiles();
        let profiles = profiles_config.latest_ref();
        let current = profiles.get_current();
        logging!(info, Type::Config, true, "当前配置: {:?}, 目标配置: {}", current, uid);
        current
    };
    // 如果当前配置与目标配置相同，跳过切换
    if let Some(ref current) = current_uid {
        if current == &uid {
            logging!(info, Type::Config, true, "当前配置与目标配置相同 ({}), 跳过配置切换", uid);
            return Ok(());
        }
    }
    
    // 创建切换配置的请求
    let mut profiles_patch = IProfiles::default();
    profiles_patch.current = Some(uid.clone());
    
    logging!(info, Type::Config, true, "开始执行配置切换...");
    
    // 执行配置切换
    match patch_profiles_config(profiles_patch).await {
        Ok(success) => {
            if success {
                logging!(info, Type::Config, true, "配置切换成功: {}", uid);
                Ok(())
            } else {
                logging!(warn, Type::Config, true, "配置切换被跳过: {}", uid);
                Ok(())
            }
        }
        Err(e) => {
            logging!(error, Type::Config, true, "配置切换失败: {} - {}", uid, e);
            Err(anyhow::anyhow!("配置切换失败: {}", e))
        }
    }
}

/// 等待核心完全启动并就绪
async fn wait_for_core_ready() {
    let max_wait_time = 30 * 1000; // 最多等待30秒
    let check_interval = 1000; // 每1000ms检查一次
    let mut elapsed = 0;
    
    logging!(info, Type::Config, true, "等待核心启动完成...");
    
    while elapsed < max_wait_time {
        let core_running = CoreManager::global().get_running_mode() != RunningMode::NotRunning;
        let running_mode = CoreManager::global().get_running_mode();
        
        logging!(debug, Type::Config, true, "核心状态检查: mode={:?}, running={}", running_mode, core_running);
        
        if core_running {
            // 核心已启动，再等待一小段时间确保IPC连接建立
            logging!(info, Type::Config, true, "核心已启动(模式: {:?})，等待IPC连接建立...", running_mode);
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            
            // 测试IPC连接是否可用
            if test_core_connection().await {
                logging!(info, Type::Config, true, "核心连接测试成功，核心已完全就绪");
                return;
            } else {
                logging!(warn, Type::Config, true, "核心连接测试失败，继续等待...");
            }
        } else {
            logging!(debug, Type::Config, true, "核心尚未启动，继续等待... (已等待{}ms)", elapsed);
        }
        
        tokio::time::sleep(tokio::time::Duration::from_millis(check_interval)).await;
        elapsed += check_interval;
        
        // 每5秒输出一次等待状态
        if elapsed % 5000 == 0 {
            logging!(info, Type::Config, true, "仍在等待核心启动... (已等待{}秒)", elapsed / 1000);
        }
    }
    
    logging!(warn, Type::Config, true, "等待核心启动超时({}秒)，继续执行配置切换", max_wait_time / 1000);
}

/// 测试核心连接是否可用
async fn test_core_connection() -> bool {
    logging!(debug, Type::Config, true, "开始测试核心连接...");
    
    // 检查IPC路径
    let ipc_path_result = crate::utils::dirs::ipc_path();
    match &ipc_path_result {
        Ok(path) => {
            logging!(debug, Type::Config, true, "测试连接使用IPC路径: {:?}", path);
            
            // 检查socket文件是否存在
            if path.exists() {
                logging!(debug, Type::Config, true, "IPC socket文件存在");
                
                // 检查文件权限（Unix系统）
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = std::fs::metadata(path) {
                        let permissions = metadata.permissions();
                        logging!(debug, Type::Config, true, "IPC socket权限: {:o}", permissions.mode());
                    }
                }
            } else {
                logging!(warn, Type::Config, true, "IPC socket文件不存在: {:?}", path);
            }
        }
        Err(e) => {
            logging!(error, Type::Config, true, "获取IPC路径失败: {}", e);
        }
    }
    
    // 尝试获取代理信息来测试IPC连接
    logging!(debug, Type::Config, true, "尝试通过IPC获取代理信息...");
    match tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        IpcManager::global().get_proxies()
    ).await {
        Ok(Ok(proxies)) => {
            let proxy_count = if let Some(obj) = proxies.as_object() {
                obj.len()
            } else {
                0
            };
            logging!(info, Type::Config, true, "核心连接测试成功，获取到代理对象包含{}个字段", proxy_count);
            true
        }
        Ok(Err(e)) => {
            logging!(warn, Type::Config, true, "核心连接测试失败: {}", e);
            false
        }
        Err(_) => {
            logging!(warn, Type::Config, true, "核心连接测试超时(3秒)");
            false
        }
    }
}

/// 启动时自动启用跟随系统启动功能
/// 每次应用启动时检查并自动设置跟随系统启动功能
async fn auto_enable_autostart_on_system_startup() -> Result<()> {
    logging!(info, Type::Setup, true, "检查并自动启用跟随系统启动功能...");
    
    // 使用 scopeguard 确保错误不影响启动流程
    let _guard = scopeguard::guard((), |_| {
        logging!(trace, Type::Setup, true, "自动启用跟随系统启动功能检查完成");
    });
    
    // 获取当前跟随系统启动状态
    let current_state = {
        Config::verge().latest_ref().enable_auto_launch.unwrap_or(false)
    };
    
    // 如果已经启用，跳过设置
    if current_state {
        logging!(info, Type::Setup, true, "跟随系统启动功能已启用，跳过自动设置");
        return Ok(());
    }
    
    // // 检查是否是管理员模式（Windows下可能不支持）
    // #[cfg(target_os = "windows")]
    // {
    //     if check_is_admin() {
    //         logging!(
    //             info,
    //             Type::Setup,
    //             true,
    //             "检测到管理员模式，跳过自动启用跟随系统启动功能"
    //         );
    //         return Ok(());
    //     }
    // }
    
    // 检查平台支持
    let platform_supported = check_platform_autostart_support();
    if !platform_supported {
        logging!(
            warn,
            Type::Setup,
            true,
            "当前平台不支持自动启用跟随系统启动功能"
        );
        return Ok(());
    }
    
    // 自动启用跟随系统启动
    logging!(info, Type::Setup, true, "自动启用跟随系统启动功能...");
    
    // 使用现有的配置更新机制
    let patch = IVerge {
        enable_auto_launch: Some(true),
        ..Default::default()
    };
    match feat::patch_verge(patch, false).await {
        Ok(_) => {
            logging!(info, Type::Setup, true, "跟随系统启动功能已自动启用");
        }
        Err(e) => {
            logging!(
                warn,
                Type::Setup,
                true,
                "自动启用跟随系统启动功能失败，但不影响应用启动: {}",
                e
            );
            // 返回错误，但在调用处会被捕获并记录，不会中断启动
            return Err(e);
        }
    }
    
    Ok(())
}

/// 检查平台是否支持跟随系统启动功能
fn check_platform_autostart_support() -> bool {
    #[cfg(target_os = "windows")]
    {
        true
        // // Windows: 检查是否有写入启动文件夹的权限
        // use crate::utils::autostart::get_startup_dir;
        // match get_startup_dir() {
        //     Ok(_) => true,
        //     Err(e) => {
        //         logging!(
        //             warn,
        //             Type::Setup,
        //             true,
        //             "Windows启动文件夹访问失败: {}",
        //             e
        //         );
        //         false
        //     }
        // }
    }
    
    #[cfg(target_os = "macos")]
    {
        // macOS: 通常都支持 LaunchAgent
        true
    }
    
    #[cfg(target_os = "linux")]
    {
        // Linux: 检查桌面环境和 autostart 目录
        let has_xdg_config = std::env::var("XDG_CONFIG_HOME").is_ok();
        let has_home = std::env::var("HOME").is_ok();
        
        if !has_xdg_config && !has_home {
            logging!(
                warn,
                Type::Setup,
                true,
                "Linux环境缺少必要的环境变量(XDG_CONFIG_HOME或HOME)"
            );
            false
        } else {
            true
        }
    }
    
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        logging!(
            warn,
            Type::Setup,
            true,
            "未知平台，不支持自动启用跟随系统启动功能"
        );
        false
    }
}

// /// 检查是否以管理员身份运行（内部使用版本）
// #[cfg(target_os = "windows")]
// fn check_is_admin() -> bool {
//     use deelevate::{PrivilegeLevel, Token};
    
//     Token::with_current_process()
//         .and_then(|token| token.privilege_level())
//         .map(|level| level != PrivilegeLevel::NotPrivileged)
//         .unwrap_or(false)
// }

// /// 测试启动时自动导入订阅功能
// pub async fn test_auto_import_startup_urls() -> Result<()> {
//     logging!(info, Type::Config, true, "手动测试启动时自动导入订阅功能");
//     auto_import_startup_urls().await;
//     Ok(())
// }

/// 在配置导入成功后自动开启系统代理
/// 
/// ## 功能说明
/// 
/// 当启动时自动导入订阅配置成功并切换到新配置后，
/// 自动启用系统代理模式，让用户可以立即使用代理服务。
/// 
/// ## 实现特性
/// 
/// - 检查当前系统代理状态，避免重复开启
/// - 使用现有的配置更新机制确保一致性
/// - 完整的错误处理和日志记录
/// - 不会阻塞配置切换流程
/// - 更新前端界面和系统托盘状态
/// 
/// ## 使用场景
/// 
/// - 首次启动导入配置后立即可用
/// - 企业环境下的自动化配置部署
/// - 减少用户手动操作步骤
/// 
async fn auto_enable_system_proxy_after_import() -> Result<()> {
    logging!(info, Type::Config, true, "开始自动启用系统代理...");
    
    // 检查当前系统代理状态
    let current_system_proxy_enabled = {
        let verge = Config::verge();
        let verge_config = verge.latest_ref();
        verge_config.enable_system_proxy.unwrap_or(false)
    };
    
    // 如果系统代理已经启用，跳过设置
    if current_system_proxy_enabled {
        logging!(info, Type::Config, true, "系统代理已启用，跳过自动设置");
        return Ok(());
    }
    
    logging!(info, Type::Config, true, "系统代理未启用，开始自动启用...");
    
    // 使用现有的配置更新机制启用系统代理
    let patch = IVerge {
        enable_system_proxy: Some(true),
        ..Default::default()
    };
    
    match feat::patch_verge(patch, false).await {
        Ok(_) => {
            logging!(info, Type::Config, true, "系统代理已自动启用");
            Ok(())
        }
        Err(e) => {
            logging!(error, Type::Config, true, "自动启用系统代理失败: {}", e);
            Err(anyhow::anyhow!("自动启用系统代理失败: {}", e))
        }
    }
}

fn refresh_ui_after_config_change(
    refresh_clash: bool,
    refresh_verge: bool, 
    update_tray_menu: bool,
    update_tray_icon: bool,
    delay_ms: u64
) {
    // 在内部使用AsyncHandler::spawn处理异步操作
    AsyncHandler::spawn(move || async move {
        // 延迟确保配置已完全应用
        if delay_ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }
        
        logging!(info, Type::Config, true, "开始统一刷新UI界面");
        
        // 1. 刷新Clash界面（代理列表等）
        if refresh_clash {
            logging!(info, Type::Config, true, "刷新Clash界面");
            handle::Handle::refresh_clash();
            
            // 短暂间隔避免冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        
        // 2. 刷新Verge界面（设置等）
        if refresh_verge {
            logging!(info, Type::Config, true, "刷新Verge界面");
            handle::Handle::refresh_verge();
            
            // 短暂间隔避免冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        
        // 3. 更新系统托盘菜单
        if update_tray_menu {
            logging!(info, Type::Config, true, "更新系统托盘菜单");
            if let Err(e) = tray::Tray::global().update_menu() {
                logging!(warn, Type::Config, true, "更新托盘菜单失败: {}", e);
            } else {
                logging!(info, Type::Config, true, "托盘菜单更新成功");
            }
            
            // 短暂间隔避免冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        
        // 4. 更新托盘图标状态
        if update_tray_icon {
            logging!(info, Type::Config, true, "更新托盘图标状态");
            if let Err(e) = tray::Tray::global().update_part() {
                logging!(warn, Type::Config, true, "更新托盘图标失败: {}", e);
            } else {
                logging!(info, Type::Config, true, "托盘图标更新成功");
            }
        }
        
        logging!(info, Type::Config, true, "UI界面刷新完成");
    });
}
