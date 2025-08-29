extern crate warp;

use super::resolve;
use crate::{
    config::{Config, IVerge, DEFAULT_PAC},
    logging_error,
    process::AsyncHandler,
    utils::logging::Type,
};
use anyhow::{bail, Result};
use port_scanner::local_port_available;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::Notify;
use warp::Filter;

// 全局的服务器关闭信号
static SERVER_SHUTDOWN: once_cell::sync::Lazy<Arc<Notify>> = 
    once_cell::sync::Lazy::new(|| Arc::new(Notify::new()));

/// 关闭内置服务器
pub fn shutdown_embed_server() {
    log::info!("发送内置服务器关闭信号");
    SERVER_SHUTDOWN.notify_waiters();
}

#[derive(serde::Deserialize, Debug)]
struct QueryParam {
    param: String,
}

/// check whether there is already exists
pub async fn check_singleton() -> Result<()> {
    let port = IVerge::get_singleton_port();
    if !local_port_available(port) {
        let argvs: Vec<String> = std::env::args().collect();
        if argvs.len() > 1 {
            #[cfg(not(target_os = "macos"))]
            {
                let param = argvs[1].as_str();
                if param.starts_with("clash:") {
                    let _ = reqwest::get(format!(
                        "http://127.0.0.1:{port}/commands/scheme?param={param}"
                    ))
                    .await;
                }
            }
        } else {
            let _ = reqwest::get(format!("http://127.0.0.1:{port}/commands/visible")).await;
        }
        log::error!("failed to setup singleton listen server");
        bail!("app exists");
    }
    Ok(())
}

/// The embed server only be used to implement singleton process
/// maybe it can be used as pac server later
pub fn embed_server() {
    let port = IVerge::get_singleton_port();

    AsyncHandler::spawn(move || async move {
        let visible = warp::path!("commands" / "visible").map(|| {
            resolve::create_window(false);
            warp::reply::with_status("ok".to_string(), warp::http::StatusCode::OK)
        });

        let pac = warp::path!("commands" / "pac").map(|| {
            let content = Config::verge()
                .latest_ref()
                .pac_file_content
                .clone()
                .unwrap_or(DEFAULT_PAC.to_string());
            let port = Config::verge()
                .latest_ref()
                .verge_mixed_port
                .unwrap_or(Config::clash().latest_ref().get_mixed_port());
            let content = content.replace("%mixed-port%", &format!("{port}"));
            warp::http::Response::builder()
                .header("Content-Type", "application/x-ns-proxy-autoconfig")
                .body(content)
                .unwrap_or_default()
        });
        async fn scheme_handler(query: QueryParam) -> Result<String, Infallible> {
            logging_error!(
                Type::Setup,
                true,
                resolve::resolve_scheme(query.param).await
            );
            Ok("ok".to_string())
        }

        let scheme = warp::path!("commands" / "scheme")
            .and(warp::query::<QueryParam>())
            .and_then(scheme_handler);
        let commands = visible.or(scheme).or(pac);
        
        // 检查端口是否可用
        if !local_port_available(port) {
            log::error!("端口 {} 被占用，内嵌服务器启动失败", port);
            return;
        }
        
        // 启动服务器
        log::info!("启动内嵌服务器，端口: {}", port);
        
        // 创建服务器future
        let server_future = warp::serve(commands).run(([127, 0, 0, 1], port));
        
        log::info!("内嵌服务器启动成功，端口: {}", port);
        
        // 使用 tokio::select! 等待服务器运行或关闭信号
        tokio::select! {
            _ = server_future => {
                log::info!("内嵌服务器正常结束");
            }
            _ = SERVER_SHUTDOWN.notified() => {
                log::info!("收到关闭信号，内嵌服务器将在当前连接处理完毕后关闭");
            }
        }
        
        log::info!("内嵌服务器已完全关闭");
    });
}
