//! Desktop-independent Quark API, encrypted session and settings.
mod files;
mod session;
mod store;

use reqwest::{header, redirect::Policy, Client, Method, Url};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const API_ORIGIN: &str = "https://drive-pc.quark.cn";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.6723.191 Safari/537.36 QuarkPC/4.1.5.397 quark-cloud-drive/2.5.40 Channel/pckk_other_ch";
pub const UC_PARAMS: &str = "dsdnfrpfbivesscpgimibtbmnijblauputogpintnwktprchmt";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Error {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub biz_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}
impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            biz_code: None,
            http_status: None,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}
struct Inner {
    profile: PathBuf,
    client: Client,
    state: Mutex<State>,
}
struct State {
    store: store::LocalStore,
    document: Value,
}

impl Core {
    pub fn open(profile: impl AsRef<Path>) -> Result<Self> {
        let profile = profile.as_ref().to_owned();
        let (store, document) = store::LocalStore::open(&profile)?;
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .retry(reqwest::retry::never())
            .tcp_keepalive(Duration::from_secs(30))
            .pool_max_idle_per_host(16)
            .build()
            .map_err(|_| Error::new("NETWORK", "网络客户端初始化失败"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                profile,
                client,
                state: Mutex::new(State { store, document }),
            }),
        })
    }
    pub fn http_client(&self) -> Client {
        self.inner.client.clone()
    }
    pub fn profile_dir(&self) -> PathBuf {
        self.inner.profile.clone()
    }
    pub fn cookie_header(&self) -> String {
        session::cookie_header(&self.inner.state.lock().unwrap().document)
    }
    pub fn settings(&self) -> Value {
        session::settings(&self.inner.state.lock().unwrap().document)
    }
    pub fn replace_cookies(&self, cookies: Value) -> Result<()> {
        let cookies = session::normalize_external(cookies)?;
        self.update(|doc| {
            doc["cookies"] = cookies;
            doc["expired"] = json!(false);
            doc["account"] = Value::Null;
        })
    }
    fn update(&self, change: impl FnOnce(&mut Value)) -> Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        let mut next = state.document.clone();
        change(&mut next);
        next["savedAt"] = json!(session::now_ms());
        state.store.write(&next)?;
        state.document = next;
        Ok(())
    }
    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.request(Method::GET, path, query, None).await
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request(Method::POST, path, &[], Some(body)).await
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let url = api_url(path, query)?;
        if self.cookie_header().is_empty() {
            return Err(Error::new("NEED_LOGIN", "请先登录夸克网盘"));
        }
        let tries = if method == Method::GET { 3 } else { 1 };
        for attempt in 0..tries {
            let result = self
                .request_once(method.clone(), url.clone(), body.as_ref())
                .await;
            if attempt + 1 == tries
                || result.as_ref().err().is_none_or(|e| {
                    !["NETWORK", "TIMEOUT", "SERVER", "RATE_LIMIT"].contains(&e.code.as_str())
                })
            {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(300 * (1u64 << attempt))).await;
        }
        unreachable!()
    }
    async fn request_once(&self, method: Method, url: Url, body: Option<&Value>) -> Result<Value> {
        let mut req = self
            .inner
            .client
            .request(method, url)
            .timeout(Duration::from_secs(30))
            .header(header::REFERER, "https://pan.quark.cn/")
            .header(header::ORIGIN, "https://pan.quark.cn")
            .header(header::ACCEPT, "application/json, text/plain, */*")
            .header(header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8");
        let cookie = self.cookie_header();
        if !cookie.is_empty() {
            let mut sensitive = header::HeaderValue::from_str(&cookie)
                .map_err(|_| Error::new("INVALID_ARG", "登录资料包含无效 Cookie"))?;
            sensitive.set_sensitive(true);
            req = req.header(header::COOKIE, sensitive);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        // POST is never automatically replayed: a timeout may follow a committed write.
        let mut response = req.send().await.map_err(network_error)?;
        let status = response.status().as_u16();
        let set_cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|h| h.to_str().ok().map(str::to_owned))
            .collect::<Vec<_>>();
        if !set_cookies.is_empty() {
            self.update(|doc| session::merge_set_cookie(doc, &set_cookies))?;
        }
        const MAX_METADATA: usize = 8 * 1024 * 1024;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_METADATA as u64)
        {
            return Err(Error::new("SERVER", "服务端元数据响应过大"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_METADATA {
                return Err(Error::new("SERVER", "服务端元数据响应过大"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let env: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("SERVER", "服务端未返回有效 JSON"))?;
        if let Some(error) = envelope_error(status, &env) {
            if error.code == "NEED_LOGIN" {
                self.update(|doc| doc["expired"] = json!(true))?;
            }
            return Err(error);
        }
        Ok(env)
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "auth.status" => Ok(session::account_state(
                &self.inner.state.lock().unwrap().document,
            )),
            "auth.verify" => {
                let state = self.call_status();
                if self.cookie_header().is_empty()
                    || (state["expired"] == true && params["force"] != true)
                {
                    return Ok(state);
                }
                match self.get("/1/clouddrive/config", &[]).await {
                    Ok(_) => {
                        self.update(|doc| doc["expired"] = json!(false))?;
                        Ok(self.call_status())
                    }
                    Err(e) if e.code == "NEED_LOGIN" => Ok(self.call_status()),
                    Err(e) => Err(e),
                }
            }
            "auth.logout" => {
                self.update(|doc| {
                    doc["cookies"] = json!([]);
                    doc["expired"] = json!(false);
                    doc["account"] = Value::Null;
                })?;
                Ok(Value::Null)
            }
            "auth.quota" => {
                if params["dlFlow"] == true {
                    let env = self
                        .get("/1/clouddrive/quota/remain", &[("type", "DL_FLOW".into())])
                        .await?;
                    let flow = env["data"].get("DL_FLOW").unwrap_or(&env["data"]);
                    return Ok(
                        json!({"total":files::number(&flow["total"], -1),"used":files::number(&flow["used"], -1),"free":files::number(&flow["remain"], -1)}),
                    );
                }
                let env = self.get("/1/clouddrive/member", &[]).await?;
                let total = files::number(&env["data"]["total_capacity"], -1);
                let used = files::number(&env["data"]["use_capacity"], -1);
                Ok(
                    json!({"total":total,"used":used,"free":if total>=0&&used>=0 {(total-used).max(0)} else {-1}}),
                )
            }
            "auth.getUserInfo" => {
                let mut info = json!({"nickname":"","avatar":"","memberLabel":"","userId":""});
                for path in ["/1/clouddrive/member", "/1/clouddrive/config"] {
                    match self.get(path, &[]).await {
                        Ok(env) => session::merge_user_info(&mut info, &env),
                        Err(e) if e.code == "NEED_LOGIN" => return Err(e),
                        Err(_) => {}
                    }
                }
                self.update(|doc| doc["account"] = info.clone())?;
                Ok(info)
            }
            "settings.get" => Ok(self.settings()),
            "settings.set" => {
                if !params.is_object() {
                    return Err(Error::new("INVALID_ARG", "设置必须是对象"));
                }
                let next = session::merge_settings(self.settings(), params);
                self.update(|doc| doc["settings"] = next.clone())?;
                Ok(next)
            }
            _ if method.starts_with("fs.") => self.files_call(method, params).await,
            _ => Err(Error::new("INVALID_ARG", "未知核心方法")),
        }
    }
    fn call_status(&self) -> Value {
        session::account_state(&self.inner.state.lock().unwrap().document)
    }
}

fn api_url(path: &str, query: &[(&str, String)]) -> Result<Url> {
    let base = Url::parse(API_ORIGIN).unwrap();
    let mut url = base
        .join(path)
        .map_err(|_| Error::new("INVALID_ARG", "API 路径无效"))?;
    if url.origin() != base.origin()
        || !url.path().starts_with("/1/clouddrive/")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::new("FORBIDDEN", "禁止向非夸克 API 地址发送登录资料"));
    }
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("pr", "ucpro")
            .append_pair("fr", "pc")
            .append_pair("uc_param_str", UC_PARAMS)
            .append_pair("device_model", &device_model());
        for (key, value) in query {
            if !["pr", "fr", "uc_param_str", "device_model"].contains(key) {
                pairs.append_pair(key, value);
            }
        }
    }
    Ok(url)
}
fn network_error(e: reqwest::Error) -> Error {
    // reqwest::Error may include signed URLs; never propagate its Display string.
    if e.is_timeout() {
        Error::new("TIMEOUT", "请求超时，请检查网络")
    } else {
        Error::new("NETWORK", "网络请求失败，请检查网络连接和代理")
    }
}
fn envelope_error(http_status: u16, env: &Value) -> Option<Error> {
    let biz = files::number(&env["code"], 0);
    let status = files::number(&env["status"], http_status as i64);
    if http_status < 400 && status < 400 && biz == 0 {
        return None;
    }
    let message = env["message"].as_str().unwrap_or("");
    let capacity = biz == 32003
        && (message.to_ascii_lowercase().contains("capacity")
            || !env["metadata"]["missing_capacity"].is_null());
    let (code, fallback) = if capacity {
        ("NO_SPACE", "网盘容量不足")
    } else if biz == 31001 || http_status == 401 || status == 401 {
        ("NEED_LOGIN", "登录状态已失效，请重新登录")
    } else if biz == 31002 || http_status == 403 || status == 403 {
        ("FORBIDDEN", "没有权限执行该操作")
    } else if biz == 21001 || http_status == 404 || status == 404 {
        ("NOT_FOUND", "资源不存在")
    } else if http_status == 429 {
        ("RATE_LIMIT", "请求过于频繁，请稍后再试")
    } else if http_status == 409 {
        ("CONFLICT", "目标位置已存在同名文件或目录")
    } else if biz == 14001 || http_status == 400 {
        ("INVALID_ARG", "请求参数有误")
    } else if biz == 500 || http_status >= 500 {
        ("SERVER", "服务端暂时不可用，请稍后重试")
    } else {
        ("UNKNOWN", "操作失败，请重试")
    };
    let mut error = Error::new(
        code,
        if message
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        {
            message
        } else {
            fallback
        },
    );
    error.biz_code = Some(biz);
    error.http_status = Some(http_status);
    Some(error)
}
fn device_model() -> String {
    static MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MODEL
        .get_or_init(|| {
            let arch = match std::env::consts::ARCH {
                "aarch64" => "arm64",
                "x86_64" => "x64",
                other => other,
            };
            #[cfg(target_os = "windows")]
            {
                return format!(
                    "Windows 10 {}",
                    if cfg!(target_pointer_width = "64") {
                        "64-bit"
                    } else {
                        "32-bit"
                    }
                );
            }
            #[cfg(not(target_os = "windows"))]
            {
                let release = std::process::Command::new("uname")
                    .arg("-r")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .unwrap_or_default();
                format!(
                    "{} {} {}",
                    if cfg!(target_os = "macos") {
                        "macOS (Darwin)"
                    } else {
                        "Linux"
                    },
                    release.trim(),
                    arch
                )
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn api_url_rejects_credential_exfiltration_and_preserves_protocol() {
        for path in [
            "https://evil.example/1/clouddrive/config",
            "//evil.example/1/clouddrive/config",
            "http://drive-pc.quark.cn/1/clouddrive/config",
            "/else",
        ] {
            assert_eq!(api_url(path, &[]).unwrap_err().code, "FORBIDDEN");
        }
        let url = api_url("/1/clouddrive/file/search", &[("q", "A & B".into())]).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("uc_param_str").unwrap(), UC_PARAMS);
        assert_eq!(query.get("q").unwrap(), "A & B");
        assert!(!query.get("device_model").unwrap().is_empty());
    }
    #[test]
    fn capacity_is_not_rate_limit_and_unknown_code_is_not_success() {
        let err=envelope_error(400,&json!({"status":400,"code":32003,"message":"capacity limit[{0}]","metadata":{"missing_capacity":100}})).unwrap();
        assert_eq!(err.code, "NO_SPACE");
        assert_eq!(
            envelope_error(429, &json!({"code":32003})).unwrap().code,
            "RATE_LIMIT"
        );
        assert_eq!(
            envelope_error(200, &json!({"code":99999})).unwrap().code,
            "UNKNOWN"
        );
    }
    #[tokio::test]
    async fn settings_are_durable_and_corruption_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        core.call("settings.set", json!({"theme":"dark","downloadParallel":7}))
            .await
            .unwrap();
        drop(core);
        let core = Core::open(dir.path()).unwrap();
        assert_eq!(core.settings()["theme"], "dark");
        assert_eq!(core.settings()["downloadParallel"], 7);
        let file = dir.path().join("session.bin");
        let mut raw = std::fs::read(&file).unwrap();
        assert!(raw.starts_with(b"QCK2"));
        raw[20] ^= 1;
        std::fs::write(&file, &raw).unwrap();
        assert!(Core::open(dir.path()).is_err());
        assert_eq!(std::fs::read(file).unwrap(), raw);
    }
    #[tokio::test]
    async fn failed_persistence_does_not_publish_new_settings() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        let before = core.settings();
        std::fs::create_dir(dir.path().join("session.key")).unwrap();
        assert!(core
            .call("settings.set", json!({"theme":"dark"}))
            .await
            .is_err());
        assert_eq!(core.settings(), before);
    }
    #[tokio::test]
    async fn guest_file_operations_fail_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        assert_eq!(
            core.call("fs.list", json!({})).await.unwrap_err().code,
            "NEED_LOGIN"
        );
    }
}
