pub(crate) use quark_core::Result;
use quark_core::{Core, Error};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub(crate) fn err(code: &str, message: impl Into<String>) -> Error {
    Error::new(code, message.into())
}
pub(crate) fn io_err(_: std::io::Error) -> Error {
    err("LOCAL_IO", "本地文件访问失败，请检查目录权限和磁盘空间")
}
pub(crate) fn net_err(_: reqwest::Error) -> Error {
    err("NETWORK", "网络传输失败，请检查连接后重试")
}
pub(crate) fn check(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(err("ABORTED", "操作已停止"))
    } else {
        Ok(())
    }
}
pub(crate) async fn cancellable<T>(
    token: &CancellationToken,
    f: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! { biased; _ = token.cancelled() => Err(err("ABORTED", "操作已停止")), result = f => result }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Task {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub size: u64,
    pub transferred: u64,
    pub status: String,
    pub attempts: u32,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdir_fid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default)]
    pub speed: u64,
    #[serde(default = "unknown_eta")]
    pub eta: i64,
    #[serde(default)]
    pub checkpoint: Value,
    #[serde(default)]
    pub account_key: Option<String>,
    #[serde(skip)]
    pub sample: Option<(u64, u64)>,
}
fn unknown_eta() -> i64 {
    -1
}
struct Active {
    token: CancellationToken,
    done: Arc<Notify>,
    kind: String,
}
pub(crate) struct Inner {
    pub core: Core,
    tasks: Mutex<Vec<Task>>,
    active: Mutex<HashMap<String, Active>>,
    controls: AsyncMutex<()>,
    disk: Mutex<()>,
    path: PathBuf,
    started: AtomicBool,
    pub(crate) download_slots: Semaphore,
    pub(crate) upload_buffers: Semaphore,
}
#[derive(Clone)]
pub struct TransferManager {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| err("INVALID_ARG", "无效的存储目录"))?;
    std::fs::create_dir_all(parent).map_err(io_err)?;
    let temporary = parent.join(format!(".qk-{}.tmp", Uuid::new_v4()));
    let write = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&temporary).map_err(io_err)?;
        serde_json::to_writer(&mut file, value)
            .map_err(|_| err("LOCAL_IO", "任务进度序列化失败"))?;
        file.flush().map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        std::fs::rename(&temporary, path).map_err(io_err)?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(io_err)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    write
}

impl TransferManager {
    pub fn open(core: Core) -> Result<Self> {
        let path = core.profile_dir().join("transfer-tasks.json");
        let mut tasks: Vec<Task> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| err("LOCAL_IO", "任务记录损坏，原文件已保留"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io_err(e)),
        };
        for t in &mut tasks {
            if t.status == "running" || t.status == "pending" {
                t.status = "paused".into();
                t.error = Some("应用曾中断，可继续任务".into());
            }
            t.speed = 0;
            t.eta = -1;
        }
        let result = Self {
            inner: Arc::new(Inner {
                core,
                tasks: Mutex::new(tasks),
                active: Mutex::new(HashMap::new()),
                controls: AsyncMutex::new(()),
                disk: Mutex::new(()),
                path,
                started: AtomicBool::new(false),
                download_slots: Semaphore::new(32),
                upload_buffers: Semaphore::new(64 * 1024),
            }),
        };
        result.persist()?;
        Ok(result)
    }
    fn start_scheduler(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let manager = Self { inner };
                manager.pump();
            }
        });
    }
    pub fn snapshot(&self) -> Value {
        let tasks = self.inner.tasks.lock().unwrap();
        Value::Array(
            tasks
                .iter()
                .map(|t| {
                    let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
                    if let Some(o) = v.as_object_mut() {
                        o.remove("checkpoint");
                    }
                    v
                })
                .collect(),
        )
    }
    pub(crate) fn task(&self, id: &str) -> Result<Task> {
        self.inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| err("NOT_FOUND", "任务不存在"))
    }
    pub(crate) fn update(&self, id: &str, f: impl FnOnce(&mut Task), persist: bool) -> Result<()> {
        {
            let mut tasks = self.inner.tasks.lock().unwrap();
            let t = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or_else(|| err("NOT_FOUND", "任务不存在"))?;
            f(t);
            t.updated_at = now();
        }
        if persist {
            self.persist()?;
        }
        Ok(())
    }
    pub(crate) fn progress(&self, id: &str, bytes: u64, total: u64) -> Result<()> {
        self.update(
            id,
            |t| {
                let ts = now();
                if let Some((at, old)) = t.sample {
                    if ts.saturating_sub(at) >= 250 {
                        t.speed = bytes.saturating_sub(old) * 1000 / ts.saturating_sub(at).max(1);
                        t.sample = Some((ts, bytes));
                    }
                } else {
                    t.sample = Some((ts, bytes));
                }
                t.size = total;
                t.transferred = bytes;
                t.eta = if t.speed > 0 {
                    (total.saturating_sub(bytes) / t.speed) as i64
                } else {
                    -1
                };
            },
            false,
        )
    }
    pub(crate) fn checkpoint(&self, id: &str, checkpoint: Value) -> Result<()> {
        self.update(id, |t| t.checkpoint = checkpoint, true)
    }
    fn persist(&self) -> Result<()> {
        let _disk = self.inner.disk.lock().unwrap();
        let tasks = self.inner.tasks.lock().unwrap();
        atomic_json(&self.inner.path, &*tasks)
    }
    fn pump(&self) {
        let settings = self.inner.core.settings();
        let account = self.inner.core.account_key();
        let mut active = self.inner.active.lock().unwrap();
        let mut tasks = self.inner.tasks.lock().unwrap();
        for task in tasks.iter_mut().filter(|t| t.status == "pending") {
            if account.is_none() || task.account_key != account {
                task.status = "paused".into();
                task.error_code = Some("ACCOUNT_MISMATCH".into());
                task.error = Some("请登录创建此任务的账号后继续；旧版未绑定任务请重新创建".into());
                continue;
            }
            if active.contains_key(&task.id) {
                continue;
            }
            let key = if task.kind == "upload" {
                "uploadConcurrency"
            } else {
                "downloadConcurrency"
            };
            let limit = settings[key].as_u64().unwrap_or(2).clamp(1, 8) as usize;
            if active.values().filter(|a| a.kind == task.kind).count() >= limit {
                continue;
            }
            task.status = "running".into();
            task.error = None;
            task.error_code = None;
            task.attempts += 1;
            task.sample = None;
            let token = CancellationToken::new();
            let done = Arc::new(Notify::new());
            active.insert(
                task.id.clone(),
                Active {
                    token: token.clone(),
                    done: done.clone(),
                    kind: task.kind.clone(),
                },
            );
            let manager = self.clone();
            let id = task.id.clone();
            let kind = task.kind.clone();
            tokio::spawn(async move {
                let result = if kind == "upload" {
                    crate::upload::run(&manager, &id, &token).await
                } else {
                    crate::download::run(&manager, &id, &token).await
                };
                let _ = manager.update(
                    &id,
                    |t| {
                        if t.status == "running" {
                            match result {
                                Ok(()) => {
                                    t.status = "done".into();
                                    t.transferred = t.size;
                                    t.eta = 0;
                                }
                                Err(e) => {
                                    t.status = "error".into();
                                    t.error = Some(e.message);
                                    t.error_code = Some(e.code);
                                }
                            }
                        }
                        t.speed = 0;
                    },
                    true,
                );
                manager.inner.active.lock().unwrap().remove(&id);
                done.notify_waiters();
            });
        }
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.start_scheduler();
        match method {
            "transfer.list" => Ok(self.snapshot()),
            "transfer.upload" | "transfer.download" => {
                let _control = self.inner.controls.lock().await;
                if self.inner.core.account_key().is_none()
                    && !self.inner.core.cookie_header().is_empty()
                {
                    self.inner
                        .core
                        .call("auth.getUserInfo", Value::Null)
                        .await?;
                }
                let account_key = self
                    .inner
                    .core
                    .account_key()
                    .ok_or_else(|| err("NEED_LOGIN", "请先登录并验证账号，再创建传输任务"))?;
                let upload = method == "transfer.upload";
                let key = if upload { "localPaths" } else { "fids" };
                let items = params[key]
                    .as_array()
                    .ok_or_else(|| err("INVALID_ARG", "缺少传输文件列表"))?;
                if items.len() > 10000 {
                    return Err(err("INVALID_ARG", "一次最多创建 10000 个任务"));
                }
                let mut additions = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    let value = item
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| err("INVALID_ARG", "文件参数无效"))?;
                    if upload && !Path::new(value).is_absolute() {
                        return Err(err("INVALID_ARG", "上传需要绝对文件路径"));
                    }
                    let ts = now();
                    let id = Uuid::new_v4().to_string();
                    let settings = self.inner.core.settings();
                    let local_dir = params["localDir"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| settings["downloadDir"].as_str().unwrap_or(""))
                        .to_string();
                    if !upload && !Path::new(&local_dir).is_absolute() {
                        return Err(err("INVALID_ARG", "请选择下载目录"));
                    }
                    let name = if upload {
                        Path::new(value)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        crate::protocol::safe_name(params["names"][index].as_str().unwrap_or(value))
                    };
                    additions.push(Task {
                        id,
                        kind: if upload { "upload" } else { "download" }.into(),
                        name,
                        size: 0,
                        transferred: 0,
                        status: "pending".into(),
                        attempts: 0,
                        created_at: ts,
                        updated_at: ts,
                        local_path: if upload { Some(value.into()) } else { None },
                        local_dir: if upload { None } else { Some(local_dir) },
                        fid: if upload { None } else { Some(value.into()) },
                        pdir_fid: if upload {
                            Some(params["pdirFid"].as_str().unwrap_or("0").into())
                        } else {
                            None
                        },
                        error: None,
                        error_code: None,
                        speed: 0,
                        eta: -1,
                        checkpoint: json!({}),
                        account_key: Some(account_key.clone()),
                        sample: None,
                    });
                }
                let ids: Vec<_> = additions.iter().map(|t| t.id.clone()).collect();
                {
                    self.inner.tasks.lock().unwrap().extend(additions);
                }
                self.persist()?;
                Ok(json!(ids))
            }
            "transfer.pause" | "transfer.cancel" => {
                let _control = self.inner.controls.lock().await;
                let id = param_id(&params)?;
                let task = match self.task(&id) {
                    Ok(t) => t,
                    Err(_) => return Ok(Value::Null),
                };
                if task.status == "done" || task.status == "canceled" {
                    return Ok(Value::Null);
                }
                let cancel = method == "transfer.cancel";
                self.update(
                    &id,
                    |t| {
                        t.status = if cancel { "canceled" } else { "paused" }.into();
                        t.speed = 0;
                        t.eta = -1;
                    },
                    true,
                )?;
                self.wait_stop(&id).await;
                if cancel && task.kind == "download" {
                    crate::download::cleanup(&self.task(&id)?).await?;
                }
                Ok(Value::Null)
            }
            "transfer.resume" | "transfer.retry" => {
                let _control = self.inner.controls.lock().await;
                let id = param_id(&params)?;
                let t = self.task(&id)?;
                if t.account_key.is_none() || t.account_key != self.inner.core.account_key() {
                    return Err(err(
                        "ACCOUNT_MISMATCH",
                        "请登录创建此任务的账号后继续；旧版未绑定任务请重新创建",
                    ));
                }
                if t.status != "paused"
                    && t.status != "error"
                    && !(method == "transfer.retry" && t.status == "canceled")
                {
                    return Ok(Value::Null);
                }
                self.wait_stop(&id).await;
                self.update(
                    &id,
                    |t| {
                        if t.status == "canceled" && t.kind == "download" {
                            t.checkpoint = json!({});
                            t.local_path = None;
                        }
                        t.status = "pending".into();
                        t.error = None;
                        t.error_code = None;
                    },
                    true,
                )?;
                Ok(Value::Null)
            }
            "transfer.clear" => {
                let _control = self.inner.controls.lock().await;
                let list = params
                    .as_array()
                    .or_else(|| params["ids"].as_array())
                    .ok_or_else(|| err("INVALID_ARG", "缺少任务列表"))?;
                let cleanup: Vec<Task> = self
                    .inner
                    .tasks
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|t| {
                        t.kind == "download"
                            && ["error", "canceled"].contains(&t.status.as_str())
                            && list.iter().any(|id| id.as_str() == Some(&t.id))
                    })
                    .cloned()
                    .collect();
                for t in &cleanup {
                    self.wait_stop(&t.id).await;
                    crate::download::cleanup(t).await?;
                }
                let active = self.inner.active.lock().unwrap();
                self.inner.tasks.lock().unwrap().retain(|t| {
                    active.contains_key(&t.id)
                        || !["done", "error", "canceled"].contains(&t.status.as_str())
                        || !list.iter().any(|id| id.as_str() == Some(&t.id))
                });
                drop(active);
                self.persist()?;
                Ok(Value::Null)
            }
            _ => Err(err("INVALID_ARG", "未知的传输命令")),
        }
    }
    async fn wait_stop(&self, id: &str) {
        loop {
            let done = {
                let active = self.inner.active.lock().unwrap();
                active.get(id).map(|a| {
                    a.token.cancel();
                    a.done.clone()
                })
            };
            let Some(done) = done else {
                break;
            };
            let notified = done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.inner.active.lock().unwrap().contains_key(id) {
                break;
            }
            notified.await;
        }
    }
    pub async fn pause_all(&self) -> Result<()> {
        let ids: Vec<String> = self
            .inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.status == "running" || t.status == "pending")
            .map(|t| t.id.clone())
            .collect();
        for id in ids {
            self.call("transfer.pause", json!(id)).await?;
        }
        Ok(())
    }
}
fn param_id(params: &Value) -> Result<String> {
    params
        .as_str()
        .or_else(|| params["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| err("INVALID_ARG", "缺少任务 id"))
}

#[cfg(test)]
pub(crate) fn fixture(dir: &Path) -> (TransferManager, String) {
    let core = Core::open(dir.join("profile")).unwrap();
    // Synthetic local-only cookies must never be sent to the loopback download host.
    core.replace_cookies(json!([{"name":"fixture","value":"synthetic","domain":"quark.cn"}]))
        .unwrap();
    let m = TransferManager::open(core).unwrap();
    let id = Uuid::new_v4().to_string();
    let task: Task = serde_json::from_value(json!({
        "id":id,"kind":"download","name":"fixture.bin","size":0,"transferred":0,
        "status":"paused","attempts":0,"createdAt":now(),"updatedAt":now(),
        "localDir":dir.to_string_lossy(),"checkpoint":{}
    }))
    .unwrap();
    m.inner.tasks.lock().unwrap().push(task);
    m.persist().unwrap();
    (m, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn interrupted_tasks_recover_paused_and_cannot_cross_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let (m, id) = fixture(dir.path());
        m.update(
            &id,
            |t| {
                t.status = "running".into();
                t.account_key = Some("quark:old-account".into());
                t.checkpoint = json!({"fixturePart":3});
            },
            true,
        )
        .unwrap();
        let reopened = TransferManager::open(m.inner.core.clone()).unwrap();
        assert_eq!(reopened.task(&id).unwrap().status, "paused");
        assert_eq!(reopened.task(&id).unwrap().checkpoint["fixturePart"], 3);
        assert!(reopened.snapshot()[0].get("checkpoint").is_none());
        let e = reopened
            .call("transfer.resume", json!({"id":id}))
            .await
            .unwrap_err();
        assert_eq!(e.code, "ACCOUNT_MISMATCH");
        reopened
            .call("transfer.pause", json!({"id":id}))
            .await
            .unwrap();
        assert_eq!(reopened.task(&id).unwrap().status, "paused");
    }
    #[tokio::test]
    async fn missing_verified_account_cannot_enqueue_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let m = TransferManager::open(Core::open(dir.path()).unwrap()).unwrap();
        let e = m
            .call(
                "transfer.download",
                json!({"fids":["not-a-real-fid"],"localDir":dir.path()}),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code, "NEED_LOGIN");
        assert_eq!(m.snapshot(), json!([]));
    }
}
