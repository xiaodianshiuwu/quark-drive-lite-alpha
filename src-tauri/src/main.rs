#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use quark_core::{Core, Error, Result};
use quark_transfer::TransferManager;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::{Emitter, Manager};

#[derive(Clone)]
struct State {
    core: Core,
    transfers: TransferManager,
    login: Arc<AtomicBool>,
    login_generation: Arc<AtomicU64>,
    _profile_lock: Arc<std::fs::File>,
}

fn shell_error(e: impl std::fmt::Display) -> Error {
    Error::new("SYSTEM", e.to_string())
}

fn trusted(window: &tauri::WebviewWindow) -> bool {
    window.label() == "main"
        && window.url().is_ok_and(|u| {
            u.scheme() == "tauri"
                || ((u.scheme() == "http" || u.scheme() == "https")
                    && u.host_str() == Some("tauri.localhost"))
        })
}

#[tauri::command]
async fn qk_call(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    state: tauri::State<'_, State>,
    method: String,
    params: Value,
) -> Result<Value> {
    if !trusted(&window) {
        return Err(Error::new("FORBIDDEN", "只允许应用主界面调用"));
    }
    if method == "app.quit" {
        app.exit(0);
        return Ok(Value::Null);
    }
    if method == "auth.login" {
        return login(app, state.inner().clone()).await;
    }
    if method.starts_with("dialog.") {
        return dialog(&app, &method, params).await;
    }
    if method == "transfer.openItem" {
        let id = params["id"].as_str().unwrap_or_default();
        let tasks = state.transfers.snapshot();
        let task = tasks
            .as_array()
            .and_then(|a| a.iter().find(|t| t["id"] == id))
            .ok_or_else(|| Error::new("NOT_FOUND", "任务不存在"))?;
        let path = task["localPath"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| task["localDir"].as_str().map(PathBuf::from))
            .ok_or_else(|| Error::new("NOT_FOUND", "本地路径不存在"))?;
        return open_local(path, true).await;
    }
    if method == "auth.logout" {
        state.login_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(login) = app.get_webview_window("login") {
            let _ = login.close();
        }
        let tasks = state.transfers.snapshot();
        if let Some(tasks) = tasks.as_array() {
            for task in tasks {
                if matches!(task["status"].as_str(), Some("pending" | "running")) {
                    state
                        .transfers
                        .call("transfer.pause", json!({"id":task["id"]}))
                        .await?;
                }
            }
        }
        if let Some(login) = app.get_webview_window("login") {
            let _ = login.clear_all_browsing_data();
            let _ = login.close();
        }
    }
    let result = if method.starts_with("transfer.") {
        state.transfers.call(&method, params).await
    } else {
        state.core.call(&method, params).await
    };
    if method.starts_with("auth.")
        || result
            .as_ref()
            .err()
            .is_some_and(|e| e.code == "NEED_LOGIN")
    {
        if let Ok(auth) = state.core.call("auth.status", Value::Null).await {
            let _ = app.emit_to("main", "qk:auth-changed", auth);
        }
    }
    result
}

async fn login(app: tauri::AppHandle, state: State) -> Result<Value> {
    if state.login.swap(true, Ordering::SeqCst) {
        return Err(Error::new("CONFLICT", "登录窗口已打开"));
    }
    struct Reset(Arc<AtomicBool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let _reset = Reset(state.login.clone());
    let generation = state.login_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        "login",
        tauri::WebviewUrl::External("https://pan.quark.cn/".parse().unwrap()),
    )
    .title("登录夸克网盘 · 扫码后自动返回")
    .inner_size(1000., 760.)
    .incognito(true)
    .on_navigation(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|h| {
                h == "quark.cn" || h.ends_with(".quark.cn") || h == "uc.cn" || h.ends_with(".uc.cn")
            })
    })
    .build()
    .map_err(shell_error)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let result=async {
        let mut last=String::new();
        while tokio::time::Instant::now()<deadline {
            if app.get_webview_window("login").is_none() || state.login_generation.load(Ordering::SeqCst)!=generation {return Err(Error::new("ABORTED","登录已取消"));}
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let w=window.clone();
            // Cookie APIs may block on WebView2; never call them on the UI thread.
            let cookies=tauri::async_runtime::spawn_blocking(move ||w.cookies()).await.map_err(shell_error)?.map_err(shell_error)?;
            let mut seen=HashSet::new();
            let items:Vec<Value>=cookies.iter().filter(|c|{
                let d=c.domain().unwrap_or("").trim_start_matches('.');
                (d=="quark.cn"||d.ends_with(".quark.cn"))&&seen.insert(c.name().to_owned())
            }).map(|c|json!({"name":c.name(),"value":c.value(),"domain":c.domain(),"path":c.path().unwrap_or("/"),"httpOnly":c.http_only().unwrap_or(false),"secure":c.secure().unwrap_or(false),"expires":c.expires_datetime().map(|t|t.unix_timestamp())})).collect();
            let header=items.iter().map(|c|format!("{}={}",c["name"].as_str().unwrap_or(""),c["value"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("; ");
            if header.is_empty()||header==last {continue;}
            let response=state.core.http_client().get("https://drive-pc.quark.cn/1/clouddrive/config?pr=ucpro&fr=pc")
                .header("cookie",header.clone()).header("referer","https://pan.quark.cn/").header("origin","https://pan.quark.cn")
                .timeout(Duration::from_secs(20)).send().await;
            if let Ok(response)=response {
                if response.status().as_u16()==401 {last=header.clone();}
                if response.status().is_success() {
                if let Ok(envelope)=response.json::<Value>().await {if envelope["code"]==0 {
                    if app.get_webview_window("login").is_none() || state.login_generation.load(Ordering::SeqCst)!=generation {return Err(Error::new("ABORTED","登录已取消"));}
                    state.core.replace_cookies(Value::Array(items))?;
                    let auth=state.core.call("auth.verify",Value::Null).await?;
                    if state.login_generation.load(Ordering::SeqCst)!=generation {return Err(Error::new("ABORTED","登录已取消"));}
                    let _=app.emit_to("main","qk:auth-changed",&auth);
                    return Ok(auth);
                }}
            }}
        }
        Err(Error::new("TIMEOUT","登录超时，请重新扫码"))
    }.await;
    let _ = window.clear_all_browsing_data();
    let _ = window.close();
    result
}

async fn open_local(path: PathBuf, reveal: bool) -> Result<Value> {
    if !path.is_absolute() || !path.exists() {
        return Err(Error::new("NOT_FOUND", "请选择存在的本地绝对路径"));
    }
    tauri::async_runtime::spawn_blocking(move || {
        if reveal {
            #[cfg(target_os = "macos")]
            {
                return std::process::Command::new("open")
                    .arg("-R")
                    .arg(&path)
                    .status()
                    .map(|_| ())
                    .map_err(shell_error);
            }
            #[cfg(target_os = "windows")]
            {
                return std::process::Command::new("explorer")
                    .arg(format!("/select,{}", path.display()))
                    .status()
                    .map(|_| ())
                    .map_err(shell_error);
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                return open::that(path.parent().unwrap_or(&path)).map_err(shell_error);
            }
        }
        open::that(path).map_err(shell_error)
    })
    .await
    .map_err(shell_error)??;
    Ok(Value::Null)
}

async fn dialog(_app: &tauri::AppHandle, method: &str, params: Value) -> Result<Value> {
    let picker = rfd::AsyncFileDialog::new();
    match method {
        "dialog.pickFiles" => Ok(json!(picker
            .pick_files()
            .await
            .unwrap_or_default()
            .iter()
            .map(|h| h.path().to_string_lossy().to_string())
            .collect::<Vec<_>>())),
        "dialog.pickDirs" => Ok(json!(picker
            .pick_folders()
            .await
            .unwrap_or_default()
            .iter()
            .map(|h| h.path().to_string_lossy().to_string())
            .collect::<Vec<_>>())),
        "dialog.saveDir" => {
            let picker = if let Some(p) = params["defaultPath"].as_str().filter(|s| !s.is_empty()) {
                picker.set_directory(p)
            } else {
                picker
            };
            Ok(json!(picker
                .pick_folder()
                .await
                .map(|h| h.path().to_string_lossy().to_string())))
        }
        "dialog.confirm" => {
            let result = rfd::AsyncMessageDialog::new()
                .set_title(params["title"].as_str().unwrap_or("确认操作"))
                .set_description(format!(
                    "{}\n{}",
                    params["message"].as_str().unwrap_or(""),
                    params["detail"].as_str().unwrap_or("")
                ))
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show()
                .await;
            Ok(json!(result == rfd::MessageDialogResult::Ok))
        }
        "dialog.openPath" | "dialog.showInFolder" => {
            open_local(
                PathBuf::from(params["path"].as_str().unwrap_or("")),
                method == "dialog.showInFolder",
            )
            .await
        }
        _ => Err(Error::new("INVALID_ARG", "未知系统操作")),
    }
}

#[tauri::command]
async fn smoke_report(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    report: Value,
) -> Result<()> {
    if !cfg!(debug_assertions) || !trusted(&window) {
        return Err(Error::new("FORBIDDEN", "仅开发验收可用"));
    }
    if let Some(path) = std::env::var_os("QK_SMOKE_REPORT") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&report).map_err(shell_error)?,
        )
        .map_err(shell_error)?;
        if std::env::var_os("QK_SMOKE_EXIT").is_some() {
            app.exit(0);
        }
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![qk_call, smoke_report])
        .setup(|app| {
            let file_menu = tauri::menu::SubmenuBuilder::new(app, "文件")
                .quit_with_text("退出客户端")
                .build()?;
            let app_menu = tauri::menu::MenuBuilder::new(app)
                .item(&file_menu)
                .build()?;
            app.set_menu(app_menu)?;
            let profile = std::env::var_os("QK_PROFILE_DIR")
                .map(PathBuf::from)
                .unwrap_or(app.path().app_data_dir()?);
            std::fs::create_dir_all(&profile)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700))?;
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let profile_lock = opts.open(profile.join("app.lock"))?;
            fs2::FileExt::try_lock_exclusive(&profile_lock)
                .map_err(|_| Error::new("CONFLICT", "此资料目录已有客户端正在运行"))?;
            let core = Core::open(profile)?;
            let transfers = TransferManager::open(core.clone())?;
            let state = State {
                core,
                transfers: transfers.clone(),
                login: Arc::new(AtomicBool::new(false)),
                login_generation: Arc::new(AtomicU64::new(0)),
                _profile_lock: Arc::new(profile_lock),
            };
            let main_window = app
                .get_webview_window("main")
                .expect("main window configured");
            let close_handle = app.handle().clone();
            main_window.on_window_event(move |event| {
                if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                    close_handle.exit(0);
                }
            });
            app.manage(state.clone());
            #[cfg(debug_assertions)]
            if std::env::var_os("QK_LOGIN_ON_START").is_some() {
                let login_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = login(login_handle.clone(), state).await {
                        let _ = login_handle.emit_to(
                            "main",
                            "qk:toast",
                            json!({"level":"error","message":e.message}),
                        );
                    }
                });
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut previous = Value::Null;
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let tasks = transfers.snapshot();
                    if tasks != previous {
                        let _ = handle.emit_to("main", "qk:transfer-update", &tasks);
                        previous = tasks;
                    }
                }
            });
            #[cfg(debug_assertions)]
            if std::env::var_os("QK_SMOKE_REPORT").is_some() {
                let w = app.get_webview_window("main").unwrap();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    let _ = w.eval(include_str!("smoke.js"));
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Tauri 应用启动失败")
        .run({
            let shutting_down = Arc::new(AtomicBool::new(false));
            move |app, event| {
                if let tauri::RunEvent::ExitRequested { api, .. } = event {
                    if !shutting_down.swap(true, Ordering::SeqCst) {
                        api.prevent_exit();
                        let handle = app.clone();
                        let manager = app.state::<State>().transfers.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = manager.pause_all().await;
                            handle.exit(0);
                        });
                    }
                }
            }
        });
}
