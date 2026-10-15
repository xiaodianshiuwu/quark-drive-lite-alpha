use crate::{
    manager::{cancellable, check, err, io_err, net_err, Result, Task, TransferManager},
    protocol::{content_range, safe_name},
};
use futures_util::{stream::FuturesUnordered, StreamExt};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha1::Sha1;
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;
const PART: u64 = 1024 * 1024;
const CHECKPOINT_VERSION: u32 = 2;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DonePart {
    len: u64,
    #[serde(default)]
    sha256: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    chunk_size: u64,
    target: String,
    temp: String,
    total: u64,
    #[serde(default)]
    etag: String,
    #[serde(default)]
    parts: BTreeMap<u64, DonePart>,
    #[serde(default)]
    ready: bool,
    #[serde(default)]
    md5: String,
    #[serde(default)]
    sha1: String,
    #[serde(default)]
    sha256: String,
}
fn parse_checkpoint(t: &Task) -> Result<Checkpoint> {
    if t.checkpoint.as_object().is_some_and(|o| o.is_empty()) {
        Ok(Checkpoint::default())
    } else {
        serde_json::from_value(t.checkpoint.clone())
            .map_err(|_| err("LOCAL_IO", "下载进度格式无效，原文件已保留"))
    }
}
fn store(m: &TransferManager, id: &str, cp: &Checkpoint) -> Result<()> {
    m.checkpoint(
        id,
        serde_json::to_value(cp).map_err(|_| err("LOCAL_IO", "下载进度无法序列化"))?,
    )
}
fn path_key(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        path.nfc().collect::<String>().to_lowercase()
    } else {
        path.into()
    }
}

pub async fn run(m: &TransferManager, id: &str, token: &CancellationToken) -> Result<()> {
    let mut task = m.task(id)?;
    let fid = task
        .fid
        .clone()
        .ok_or_else(|| err("INVALID_ARG", "缺少下载文件 id"))?;
    let info = cancellable(token, m.inner.core.call("fs.info", json!(fid))).await?;
    if info["dir"].as_bool().unwrap_or(false) {
        return Err(err("INVALID_ARG", "请进入文件夹选择要下载的文件"));
    }
    let total = info["size"]
        .as_u64()
        .ok_or_else(|| err("SERVER", "服务端没有返回有效文件大小"))?;
    let name = safe_name(info["name"].as_str().unwrap_or(&task.name));
    let mut cp = parse_checkpoint(&task)?;
    if !cp.temp.is_empty() && (cp.version != CHECKPOINT_VERSION || cp.chunk_size != PART) {
        return Err(err(
            "CHECKPOINT_VERSION",
            "旧版下载分片格式不兼容，请取消此任务后重新下载；原文件已保留",
        ));
    }
    if !cp.temp.is_empty() && cp.total != total {
        return Err(err("CONFLICT", "云端文件大小已改变，请取消并重新下载"));
    }
    if cp.target.is_empty() {
        let dir = PathBuf::from(task.local_dir.as_deref().unwrap_or(""));
        fs::create_dir_all(&dir).await.map_err(io_err)?;
        cp.version = CHECKPOINT_VERSION;
        cp.chunk_size = PART;
        cp.total = total;
        cp.target = reserve(m, id, &dir, &name)
            .await?
            .to_string_lossy()
            .into_owned();
        cp.temp = format!("{}.{}.qkdownloading", cp.target, id);
        // Record ownership before creating the temporary file; cleanup never derives paths from a display name.
        store(m, id, &cp)?;
        m.update(
            id,
            |t| {
                t.local_path = Some(cp.target.clone());
                t.name = name.clone();
                t.size = total;
            },
            true,
        )?;
    }
    let temp = PathBuf::from(&cp.temp);
    validate_owned(&task, &cp)?;
    if cp.ready {
        if fs::try_exists(&cp.target).await.map_err(io_err)? {
            let existing = hash_file(Path::new(&cp.target), token).await?;
            if existing != (cp.md5.clone(), cp.sha1.clone(), cp.sha256.clone())
                || fs::metadata(&cp.target).await.map_err(io_err)?.len() != total
            {
                return Err(err("CONFLICT", "下载目标已被其他文件占用"));
            }
            let _ = fs::remove_file(&temp).await;
            let _ = fs::remove_file(format!("{}.qkreserve", cp.target)).await;
            return m.progress(id, total, total);
        }
        if fs::metadata(&temp).await.map_err(io_err)?.len() != total
            || hash_file(&temp, token).await?
                != (cp.md5.clone(), cp.sha1.clone(), cp.sha256.clone())
        {
            return Err(err(
                "CONTENT_MISMATCH",
                "已完成下载的临时文件损坏，原文件已保留",
            ));
        }
        publish(&temp, Path::new(&cp.target), token).await?;
        return m.progress(id, total, total);
    }
    let exists = fs::try_exists(&temp).await.map_err(io_err)?;
    if exists {
        let meta = fs::symlink_metadata(&temp).await.map_err(io_err)?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(err("CONFLICT", "下载临时路径已被替换"));
        }
        if meta.len() != total {
            cp.parts.clear();
        }
    } else {
        cp.parts.clear();
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&temp)
        .await
        .map_err(io_err)?;
    file.set_len(total).await.map_err(io_err)?;
    drop(file);
    verify_parts(&temp, &mut cp, token).await?;
    store(m, id, &cp)?;
    let mut link = get_link(m, &fid, token).await?;
    let mut probe = send(m, &link, Some((0, 0)), None, token).await?;
    if probe.status() == reqwest::StatusCode::FORBIDDEN {
        link = get_link(m, &fid, token).await?;
        probe = send(m, &link, Some((0, 0)), None, token).await?;
    }
    validate_encoding(&probe)?;
    let range = probe.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if range {
        if content_range(
            probe
                .headers()
                .get("content-range")
                .and_then(|s| s.to_str().ok())
                .unwrap_or(""),
        ) != Some((0, 0, total))
        {
            return Err(err("CONTENT_MISMATCH", "探测响应范围与文件大小不符"));
        }
    } else if probe.status() != reqwest::StatusCode::OK
        && !(total == 0 && probe.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE)
    {
        return Err(http_error(probe.status()));
    }
    let etag = probe
        .headers()
        .get("etag")
        .and_then(|s| s.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Ranges may only be combined when the server supplies a strong representation validator.
    // A missing/weak validator falls back to one complete response and never resumes old pieces.
    let bound_etag = strong_etag(&etag).unwrap_or("").to_string();
    if bound_etag.is_empty() || cp.etag != bound_etag {
        cp.parts.clear();
    }
    cp.etag = bound_etag;
    drop(probe);
    store(m, id, &cp)?;
    let result = if range && total > 0 && !cp.etag.is_empty() {
        multipart(m, id, &fid, &link, &temp, &mut cp, token).await
    } else {
        single(m, id, &fid, &link, &temp, &mut cp, token).await
    };
    match result {
        Err(e) if e.code == "RANGE_UNSUPPORTED" => {
            cp.parts.clear();
            store(m, id, &cp)?;
            single(m, id, &fid, &link, &temp, &mut cp, token).await?;
        }
        other => other?,
    }
    check(token)?;
    let metadata = fs::metadata(&temp).await.map_err(io_err)?;
    if metadata.len() != total {
        return Err(err("CONTENT_MISMATCH", "下载文件长度不符"));
    }
    // Hash entire finalized local file before publication. If API exposes a server digest, verify it too.
    let (md5, sha1, sha256) = hash_file(&temp, token).await?;
    for (key, value) in [("md5", &md5), ("sha1", &sha1), ("sha256", &sha256)] {
        if let Some(expected) = info[key].as_str().filter(|s| !s.is_empty()) {
            if !expected.eq_ignore_ascii_case(value) {
                return Err(err(
                    "CONTENT_MISMATCH",
                    "下载文件校验和不符，临时文件已保留",
                ));
            }
        }
    }
    cp.ready = true;
    cp.md5 = md5;
    cp.sha1 = sha1;
    cp.sha256 = sha256;
    store(m, id, &cp)?;
    publish(&temp, Path::new(&cp.target), token).await?;
    task.local_path = Some(cp.target.clone());
    m.progress(id, total, total)
}
async fn verify_parts(temp: &Path, cp: &mut Checkpoint, token: &CancellationToken) -> Result<()> {
    for (index, done) in cp.parts.clone() {
        let start = index
            .checked_mul(PART)
            .ok_or_else(|| err("LOCAL_IO", "下载分片位置溢出"))?;
        let len = PART.min(cp.total.saturating_sub(start));
        if start >= cp.total
            || done.len != len
            || hash_range(&temp, start, len, token).await? != done.sha256
        {
            cp.parts.remove(&index);
        }
    }
    Ok(())
}
async fn reserve(m: &TransferManager, id: &str, dir: &Path, name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    for i in 0..10000 {
        let candidate = dir.join(if i == 0 {
            name.to_string()
        } else {
            format!("{stem}({i}){ext}")
        });
        let key = path_key(&candidate.to_string_lossy());
        // The manager starts tasks serially, but reservations are persisted before any await.
        let reserved = m.snapshot().as_array().is_some_and(|list| {
            list.iter().any(|t| {
                t["id"].as_str() != Some(id)
                    && t["localPath"].as_str().is_some_and(|s| path_key(s) == key)
            })
        });
        if reserved || fs::try_exists(&candidate).await.map_err(io_err)? {
            continue;
        }
        // A create_new sidecar reserves the target against other processes; it contains no credentials.
        let lock = PathBuf::from(format!("{}.qkreserve", candidate.to_string_lossy()));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        match opts.open(&lock).await {
            Ok(mut f) => {
                f.write_all(id.as_bytes()).await.map_err(io_err)?;
                f.sync_all().await.map_err(io_err)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(e)),
        }
    }
    Err(err("CONFLICT", "目标目录同名文件过多"))
}
async fn get_link(m: &TransferManager, fid: &str, token: &CancellationToken) -> Result<String> {
    let env = cancellable(
        token,
        m.inner
            .core
            .post("/1/clouddrive/file/download", json!({"fids":[fid]})),
    )
    .await?;
    let list = env["data"]
        .as_array()
        .or_else(|| env["data"]["list"].as_array())
        .ok_or_else(|| err("SERVER", "服务端未返回下载链接"))?;
    list.first()
        .and_then(|r| r["download_url"].as_str().or_else(|| r["url"].as_str()))
        .map(str::to_string)
        .ok_or_else(|| err("SERVER", "服务端未返回下载链接"))
}
fn permitted_cookie_host(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|h| h == "quark.cn" || h.ends_with(".quark.cn"))
}
fn strong_etag(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    (bytes.len() >= 2
        && bytes[0] == b'"'
        && bytes[bytes.len() - 1] == b'"'
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|b| *b >= 0x21 && *b != b'"' && *b != 0x7f))
    .then_some(value)
}
fn validate_etag(response: &reqwest::Response, expected: &str) -> Result<()> {
    if !expected.is_empty()
        && response.headers().get("etag").and_then(|h| h.to_str().ok()) != Some(expected)
    {
        return Err(err(
            "CONFLICT",
            "下载对象的 ETag 已改变或丢失，未拼接不同版本",
        ));
    }
    Ok(())
}
fn validate_encoding(response: &reqwest::Response) -> Result<()> {
    if response
        .headers()
        .get("content-encoding")
        .is_some_and(|h| h.as_bytes() != b"identity")
    {
        return Err(err("CONTENT_MISMATCH", "下载响应使用了非预期压缩编码"));
    }
    Ok(())
}
fn connections(settings: &serde_json::Value, total: u64) -> usize {
    let max = settings["downloadParallel"]
        .as_u64()
        .unwrap_or(16)
        .clamp(1, 16) as usize;
    match settings["downloadStrategy"].as_str().unwrap_or("auto") {
        "sequential" => 1,
        "parallel" => max,
        _ if total <= 8 * 1024 * 1024 => max.min(total.div_ceil(PART).clamp(1, 4) as usize),
        _ if total <= 16 * 1024 * 1024 => max.min(4),
        _ => max,
    }
}
async fn send(
    m: &TransferManager,
    url: &str,
    range: Option<(u64, u64)>,
    etag: Option<&str>,
    token: &CancellationToken,
) -> Result<reqwest::Response> {
    let mut url = reqwest::Url::parse(url).map_err(|_| err("SERVER", "下载链接无效"))?;
    for _ in 0..6 {
        if url.scheme() != "https"
            && !(cfg!(test) && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
        {
            return Err(err("SERVER", "下载链接必须使用 HTTPS"));
        }
        let mut req = m
            .inner
            .core
            .http_client()
            .get(url.clone())
            .header("accept-encoding", "identity")
            .header("referer", "https://pan.quark.cn/");
        if permitted_cookie_host(&url) {
            let mut cookie =
                reqwest::header::HeaderValue::from_str(&m.inner.core.cookie_header_for_url(&url))
                    .map_err(|_| err("INVALID_ARG", "Cookie 格式无效"))?;
            cookie.set_sensitive(true);
            req = req.header("cookie", cookie);
        }
        if let Some(etag) = etag.filter(|s| !s.is_empty()) {
            req = req.header("if-match", etag);
            if range.is_some() {
                req = req.header("if-range", etag);
            }
        }
        if let Some((start, end)) = range {
            req = req.header("range", format!("bytes={start}-{end}"));
        }
        let response = cancellable(token, async {
            tokio::time::timeout(Duration::from_secs(60), req.send())
                .await
                .map_err(|_| err("NETWORK", "下载响应超时"))?
                .map_err(net_err)
        })
        .await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|s| s.to_str().ok())
                .ok_or_else(|| err("SERVER", "下载重定向缺少地址"))?;
            url = url
                .join(location)
                .map_err(|_| err("SERVER", "下载重定向地址无效"))?;
            continue;
        }
        return Ok(response);
    }
    Err(err("NETWORK", "下载重定向次数过多"))
}
fn http_error(status: reqwest::StatusCode) -> quark_core::Error {
    if status == reqwest::StatusCode::PRECONDITION_FAILED {
        err("CONFLICT", "下载过程中云端文件已改变，未拼接不同版本")
    } else if status == reqwest::StatusCode::FORBIDDEN {
        err("LINK_EXPIRED", "下载链接已过期")
    } else {
        err(
            "NETWORK",
            format!("下载服务器返回 HTTP {}", status.as_u16()),
        )
    }
}
async fn multipart(
    m: &TransferManager,
    id: &str,
    fid: &str,
    link: &str,
    temp: &Path,
    cp: &mut Checkpoint,
    token: &CancellationToken,
) -> Result<()> {
    let count = cp.total.div_ceil(PART);
    let settings = m.inner.core.settings();
    let parallel = connections(&settings, cp.total);
    let mut cursor = 0u64;
    let mut workers = FuturesUnordered::new();
    let workers_token = token.child_token();
    let mut failure = None;
    let mut complete = cp.parts.values().map(|p| p.len).sum::<u64>();
    m.progress(id, complete, cp.total)?;
    let live = Arc::new(Mutex::new((complete, BTreeMap::<u64, u64>::new())));
    loop {
        while failure.is_none() && workers.len() < parallel && cursor < count {
            let index = cursor;
            cursor += 1;
            if cp.parts.contains_key(&index) {
                continue;
            }
            let live = live.clone();
            let manager = m.clone();
            let task_id = id.to_string();
            let total = cp.total;
            let progress: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |bytes| {
                let mut live = live.lock().unwrap();
                live.1.insert(index, bytes);
                let received = live.0 + live.1.values().sum::<u64>();
                let _ = manager.progress(&task_id, received, total);
            });
            workers.push(fetch_part(
                m,
                fid,
                link,
                temp,
                index,
                cp.total,
                cp.etag.clone(),
                workers_token.clone(),
                Some(progress),
            ));
        }
        let Some(result) = workers.next().await else {
            break;
        };
        match result {
            Ok((index, done)) => {
                complete += done.len;
                {
                    let mut live = live.lock().unwrap();
                    live.0 = complete;
                    live.1.remove(&index);
                }
                cp.parts.insert(index, done);
                store(m, id, cp)?;
                m.progress(id, complete, cp.total)?;
            }
            Err(e) => {
                if failure.is_none() {
                    failure = Some(e);
                    workers_token.cancel();
                }
            }
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if cp.parts.len() as u64 != count {
        return Err(err("CONTENT_MISMATCH", "下载仍有未完成分片"));
    }
    Ok(())
}
async fn fetch_part(
    m: &TransferManager,
    fid: &str,
    link: &str,
    temp: &Path,
    index: u64,
    total: u64,
    etag: String,
    token: CancellationToken,
    on_progress: Option<Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<(u64, DonePart)> {
    let _slot = cancellable(&token, async {
        m.inner
            .download_slots
            .acquire()
            .await
            .map_err(|_| err("ABORTED", "下载已停止"))
    })
    .await?;
    let start = index * PART;
    let length = PART.min(total - start);
    let mut link = link.to_string();
    for attempt in 0..3 {
        check(&token)?;
        if let Some(progress) = &on_progress {
            progress(0);
        }
        let result = async {
            let response = send(
                m,
                &link,
                Some((start, start + length - 1)),
                Some(&etag),
                &token,
            )
            .await?;
            if response.status().is_success() {
                validate_encoding(&response)?;
                validate_etag(&response, &etag)?;
            }
            if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(if response.status() == reqwest::StatusCode::OK {
                    err("RANGE_UNSUPPORTED", "服务器忽略 Range，切换完整下载")
                } else {
                    http_error(response.status())
                });
            }
            if content_range(
                response
                    .headers()
                    .get("content-range")
                    .and_then(|s| s.to_str().ok())
                    .unwrap_or(""),
            ) != Some((start, start + length - 1, total))
            {
                return Err(err("CONTENT_MISMATCH", "分片响应范围与请求不符"));
            }
            if let Some(cl) = response.content_length() {
                if cl != length {
                    return Err(err("CONTENT_MISMATCH", "分片 Content-Length 不符"));
                }
            }
            let mut file = OpenOptions::new()
                .write(true)
                .open(temp)
                .await
                .map_err(io_err)?;
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(io_err)?;
            let mut stream = response.bytes_stream();
            let mut received = 0u64;
            let mut hash = Sha256::new();
            loop {
                let chunk = cancellable(&token, async {
                    tokio::time::timeout(Duration::from_secs(60), stream.next())
                        .await
                        .map_err(|_| err("NETWORK", "下载读取超时"))?
                        .transpose()
                        .map_err(net_err)
                })
                .await?;
                let Some(chunk) = chunk else {
                    break;
                };
                if received + chunk.len() as u64 > length {
                    return Err(err("CONTENT_MISMATCH", "分片响应超出请求范围"));
                }
                file.write_all(&chunk).await.map_err(io_err)?;
                hash.update(&chunk);
                received += chunk.len() as u64;
                if let Some(progress) = &on_progress {
                    progress(received);
                }
            }
            if received != length {
                return Err(err("CONTENT_MISMATCH", "分片响应长度不足"));
            }
            file.sync_data().await.map_err(io_err)?;
            Ok((
                index,
                DonePart {
                    len: received,
                    sha256: format!("{:x}", hash.finalize()),
                },
            ))
        }
        .await;
        match result {
            Ok(v) => return Ok(v),
            Err(e)
                if attempt < 2
                    && ["NETWORK", "LINK_EXPIRED", "CONTENT_MISMATCH"]
                        .contains(&e.code.as_str()) =>
            {
                if e.code == "LINK_EXPIRED" {
                    link = get_link(m, fid, &token).await?;
                }
                cancellable(&token, async {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                    Ok(())
                })
                .await?;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
async fn single(
    m: &TransferManager,
    id: &str,
    fid: &str,
    link: &str,
    temp: &Path,
    cp: &mut Checkpoint,
    token: &CancellationToken,
) -> Result<()> {
    let _slot = cancellable(token, async {
        m.inner
            .download_slots
            .acquire()
            .await
            .map_err(|_| err("ABORTED", "下载已停止"))
    })
    .await?;
    let mut link = link.to_string();
    cp.parts.clear();
    store(m, id, cp)?;
    for attempt in 0..3 {
        let result = async {
            let response = send(m, &link, None, Some(&cp.etag), token).await?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(http_error(response.status()));
            }
            validate_encoding(&response)?;
            validate_etag(&response, &cp.etag)?;
            if let Some(cl) = response.content_length() {
                if cl != cp.total {
                    return Err(err(
                        "CONTENT_MISMATCH",
                        "下载 Content-Length 与文件大小不符",
                    ));
                }
            }
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(temp)
                .await
                .map_err(io_err)?;
            let mut stream = response.bytes_stream();
            let mut received = 0u64;
            loop {
                let chunk = cancellable(token, async {
                    tokio::time::timeout(Duration::from_secs(60), stream.next())
                        .await
                        .map_err(|_| err("NETWORK", "下载读取超时"))?
                        .transpose()
                        .map_err(net_err)
                })
                .await?;
                let Some(chunk) = chunk else {
                    break;
                };
                if received + chunk.len() as u64 > cp.total {
                    return Err(err("CONTENT_MISMATCH", "下载响应超过文件大小"));
                }
                file.write_all(&chunk).await.map_err(io_err)?;
                received += chunk.len() as u64;
                m.progress(id, received, cp.total)?;
            }
            if received != cp.total {
                return Err(err("CONTENT_MISMATCH", "下载响应不完整"));
            }
            file.sync_all().await.map_err(io_err)?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(e)
                if attempt < 2
                    && ["NETWORK", "LINK_EXPIRED", "CONTENT_MISMATCH"]
                        .contains(&e.code.as_str()) =>
            {
                if e.code == "LINK_EXPIRED" {
                    link = get_link(m, fid, token).await?;
                }
                cancellable(token, async {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                    Ok(())
                })
                .await?;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
async fn hash_range(
    path: &Path,
    start: u64,
    len: u64,
    token: &CancellationToken,
) -> Result<String> {
    let mut file = fs::File::open(path).await.map_err(io_err)?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(io_err)?;
    let mut hash = Sha256::new();
    let mut left = len;
    let mut buf = vec![0; 1024 * 1024];
    while left > 0 {
        check(token)?;
        let n = file
            .read(&mut buf[..(left as usize).min(1024 * 1024)])
            .await
            .map_err(io_err)?;
        if n == 0 {
            return Ok(String::new());
        }
        hash.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(format!("{:x}", hash.finalize()))
}
async fn hash_file(path: &Path, token: &CancellationToken) -> Result<(String, String, String)> {
    let mut file = fs::File::open(path).await.map_err(io_err)?;
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        check(token)?;
        let n = file.read(&mut buf).await.map_err(io_err)?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha1.update(&buf[..n]);
        sha256.update(&buf[..n]);
    }
    Ok((
        format!("{:x}", md5.finalize()),
        format!("{:x}", sha1.finalize()),
        format!("{:x}", sha256.finalize()),
    ))
}
fn validate_owned(task: &Task, cp: &Checkpoint) -> Result<()> {
    if cp.target.is_empty()
        || cp.temp != format!("{}.{}.qkdownloading", cp.target, task.id)
        || task
            .local_dir
            .as_deref()
            .is_none_or(|dir| Path::new(&cp.target).parent() != Some(Path::new(dir)))
    {
        return Err(err("CONFLICT", "下载临时文件所有权记录不符"));
    }
    Ok(())
}
async fn publish(temp: &Path, target: &Path, token: &CancellationToken) -> Result<()> {
    check(token)?;
    match fs::hard_link(temp, target).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(err("CONFLICT", "目标文件已存在，下载没有覆盖它"));
        }
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::EPERM) | Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::EINVAL)
            ) =>
        {
            let source = temp.to_owned();
            let destination = target.to_owned();
            tokio::task::spawn_blocking(move || rename_no_replace(&source, &destination))
                .await
                .map_err(|_| err("LOCAL_IO", "发布下载文件失败"))??;
        }
        Err(e) => return Err(io_err(e)),
    }
    // Both link and native exclusive rename publish a complete file atomically without replacing user data.
    if fs::try_exists(temp).await.map_err(io_err)? {
        fs::remove_file(temp).await.map_err(io_err)?;
    }
    let _ = fs::remove_file(format!("{}.qkreserve", target.to_string_lossy())).await;
    Ok(())
}
pub async fn cleanup(task: &Task) -> Result<()> {
    let cp = parse_checkpoint(task)?;
    if cp.temp.is_empty() {
        return Ok(());
    }
    validate_owned(task, &cp)?;
    match fs::remove_file(&cp.temp).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(e)),
    }
    let lock = format!("{}.qkreserve", cp.target);
    if fs::read_to_string(&lock).await.ok().as_deref() == Some(&task.id) {
        fs::remove_file(lock).await.map_err(io_err)?;
    }
    Ok(())
}

fn rename_no_replace(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| err("INVALID_ARG", "文件路径包含无效字符"))?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| err("INVALID_ARG", "文件路径包含无效字符"))?;
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let result = -1;
        if result == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(err("CONFLICT", "目标文件已存在，下载没有覆盖它"));
        }
        return Err(io_err(e));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) } != 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(err("CONFLICT", "目标文件已存在，下载没有覆盖它"));
        }
        Err(io_err(e))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (source, destination);
        Err(err("LOCAL_IO", "当前文件系统不支持安全发布"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Server {
        url: String,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Server {
        fn new(reply: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/fixture", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let reply = Arc::new(reply);
            let thread = std::thread::spawn(move || {
                let mut connections = Vec::new();
                while !flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            // macOS accept inherits O_NONBLOCK from the listener.
                            socket.set_nonblocking(false).unwrap();
                            socket
                                .set_write_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let reply = reply.clone();
                            connections.push(std::thread::spawn(move || {
                                socket
                                    .set_read_timeout(Some(Duration::from_secs(3)))
                                    .unwrap();
                                let mut input = Vec::new();
                                let mut buf = [0; 1024];
                                while !input.ends_with(b"\r\n\r\n") && input.len() < 16384 {
                                    let n = socket.read(&mut buf).unwrap_or(0);
                                    if n == 0 {
                                        break;
                                    }
                                    input.extend_from_slice(&buf[..n]);
                                }
                                let response =
                                    reply(&String::from_utf8_lossy(&input).to_ascii_lowercase());
                                let _ = socket.write_all(&response);
                            }));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(2)),
                    }
                }
                for thread in connections {
                    thread.join().unwrap();
                }
            });
            Self {
                url,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    fn response(status: &str, headers: &str, bytes: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            bytes.len()
        )
        .into_bytes();
        out.extend_from_slice(bytes);
        out
    }
    async fn prepare(dir: &Path, total: u64) -> (TransferManager, String, PathBuf, Checkpoint) {
        let (m, id) = crate::manager::fixture(dir);
        let target = dir.join("fixture.bin");
        let temp = PathBuf::from(format!("{}.{}.qkdownloading", target.display(), id));
        fs::File::create(&temp)
            .await
            .unwrap()
            .set_len(total)
            .await
            .unwrap();
        let cp = Checkpoint {
            version: CHECKPOINT_VERSION,
            chunk_size: PART,
            target: target.to_string_lossy().into_owned(),
            temp: temp.to_string_lossy().into_owned(),
            total,
            etag: "\"fixture-v1\"".into(),
            ..Default::default()
        };
        store(&m, &id, &cp).unwrap();
        (m, id, temp, cp)
    }
    #[test]
    fn strategy_is_size_adaptive_and_respects_user_limit() {
        let settings = json!({"downloadParallel":16,"downloadStrategy":"auto"});
        assert_eq!(connections(&settings, PART), 1);
        assert_eq!(connections(&settings, 8 * PART), 4);
        assert_eq!(connections(&settings, 16 * PART + 1), 16);
        assert_eq!(connections(&json!({"downloadParallel":2}), 100 * PART), 2);
        assert_eq!(
            connections(
                &json!({"downloadParallel":16,"downloadStrategy":"sequential"}),
                100 * PART
            ),
            1
        );
        for invalid in ["", "W/\"v1\"", "v1", "\"bad\"quote\"", "\"bad\n\""] {
            assert_eq!(strong_etag(invalid), None);
        }
        assert_eq!(strong_etag("\"v1\""), Some("\"v1\""));
    }
    #[tokio::test]
    async fn real_range_stream_binds_etag_and_never_sends_cookies_to_cdn() {
        let bytes = vec![0x5a; PART as usize + 17];
        let serve = bytes.clone();
        let server = Server::new(move |request| {
            assert!(request.contains("range: bytes=1048576-1048592\r\n"));
            assert!(request.contains("if-match: \"fixture-v1\"\r\n"));
            assert!(request.contains("if-range: \"fixture-v1\"\r\n"));
            assert!(!request.contains("cookie:"));
            response(
                "206 Partial Content",
                "ETag: \"fixture-v1\"\r\nContent-Range: bytes 1048576-1048592/1048593\r\n",
                &serve[PART as usize..],
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let (m, _, temp, _) = prepare(dir.path(), bytes.len() as u64).await;
        let token = CancellationToken::new();
        let (i, done) = fetch_part(
            &m,
            "fixture",
            &server.url,
            &temp,
            1,
            bytes.len() as u64,
            "\"fixture-v1\"".into(),
            token.clone(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(i, 1);
        assert_eq!(done.len, 17);
        assert_eq!(
            done.sha256,
            hash_range(&temp, PART, 17, &token).await.unwrap()
        );
        assert_eq!(
            &fs::read(temp).await.unwrap()[PART as usize..],
            &bytes[PART as usize..]
        );
    }
    #[tokio::test]
    async fn changed_or_missing_etag_and_wrong_range_never_publish() {
        for (headers,code) in [
            ("ETag: \"fixture-v2\"\r\nContent-Range: bytes 0-15/16\r\n","CONFLICT"),
            ("Content-Range: bytes 0-15/16\r\n","CONFLICT"),
            ("ETag: \"fixture-v1\"\r\nContent-Range: bytes 1-16/17\r\n","CONTENT_MISMATCH"),
            ("ETag: \"fixture-v1\"\r\nContent-Encoding: gzip\r\nContent-Range: bytes 0-15/16\r\n","CONTENT_MISMATCH"),
        ] {
            let server=Server::new(move |_| response("206 Partial Content",headers,&[3;16]));
            let dir=tempfile::tempdir().unwrap();
            let (m,_,temp,cp)=prepare(dir.path(),16).await;
            let e=fetch_part(&m,"fixture",&server.url,&temp,0,16,cp.etag,CancellationToken::new(),None).await.unwrap_err();
            assert_eq!(e.code,code); assert!(!Path::new(&cp.target).exists());
        }
    }
    #[tokio::test]
    async fn ignored_range_falls_back_to_complete_response_and_exclusive_publication() {
        let server = Server::new(|_| response("200 OK", "ETag: \"fixture-v1\"\r\n", &[9; 32]));
        let dir = tempfile::tempdir().unwrap();
        let (m, id, temp, mut cp) = prepare(dir.path(), 32).await;
        let token = CancellationToken::new();
        let e = fetch_part(
            &m,
            "fixture",
            &server.url,
            &temp,
            0,
            32,
            cp.etag.clone(),
            token.clone(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "RANGE_UNSUPPORTED");
        single(&m, &id, "fixture", &server.url, &temp, &mut cp, &token)
            .await
            .unwrap();
        assert_eq!(fs::read(&temp).await.unwrap(), vec![9; 32]);
        fs::write(&cp.target, b"original user data").await.unwrap();
        assert_eq!(
            publish(&temp, Path::new(&cp.target), &token)
                .await
                .unwrap_err()
                .code,
            "CONFLICT"
        );
        assert_eq!(fs::read(&cp.target).await.unwrap(), b"original user data");
        fs::remove_file(&cp.target).await.unwrap();
        publish(&temp, Path::new(&cp.target), &token).await.unwrap();
        assert_eq!(fs::read(&cp.target).await.unwrap(), vec![9; 32]);
        assert!(!temp.exists());
    }
    #[tokio::test]
    async fn durable_resume_skips_verified_parts_and_redownloads_corrupted_parts() {
        let bytes = Arc::new(
            (0..(2 * PART + 29))
                .map(|i| (i % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let received = Arc::new(Mutex::new(Vec::new()));
        let served = bytes.clone();
        let requests = received.clone();
        let server = Server::new(move |request| {
            let range = request
                .lines()
                .find_map(|s| s.strip_prefix("range: bytes="))
                .unwrap();
            let (start, end) = range.split_once('-').unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap();
            requests.lock().unwrap().push(start as u64 / PART);
            response(
                "206 Partial Content",
                &format!(
                    "ETag: \"fixture-v1\"\r\nContent-Range: bytes {start}-{end}/{}\r\n",
                    served.len()
                ),
                &served[start..=end],
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let (m, id, temp, mut cp) = prepare(dir.path(), bytes.len() as u64).await;
        let token = CancellationToken::new();
        let (_, done) = fetch_part(
            &m,
            "fixture",
            &server.url,
            &temp,
            0,
            cp.total,
            cp.etag.clone(),
            token.clone(),
            None,
        )
        .await
        .unwrap();
        cp.parts.insert(0, done);
        store(&m, &id, &cp).unwrap();
        let reopened = TransferManager::open(m.inner.core.clone()).unwrap();
        let mut restored = parse_checkpoint(&reopened.task(&id).unwrap()).unwrap();
        verify_parts(&temp, &mut restored, &token).await.unwrap();
        multipart(
            &reopened,
            &id,
            "fixture",
            &server.url,
            &temp,
            &mut restored,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(
            received.lock().unwrap().iter().filter(|i| **i == 0).count(),
            1
        );
        assert_eq!(fs::read(&temp).await.unwrap(), *bytes);
        let mut file = OpenOptions::new().write(true).open(&temp).await.unwrap();
        file.write_all(&[255; 32]).await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);
        verify_parts(&temp, &mut restored, &token).await.unwrap();
        assert!(!restored.parts.contains_key(&0));
        assert_eq!(restored.parts.len(), 2);
        multipart(
            &reopened,
            &id,
            "fixture",
            &server.url,
            &temp,
            &mut restored,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(
            received.lock().unwrap().iter().filter(|i| **i == 0).count(),
            2
        );
        assert_eq!(
            hash_file(&temp, &token).await.unwrap().2,
            format!("{:x}", Sha256::digest(&*bytes))
        );
    }
    #[tokio::test]
    async fn cancelled_stream_stops_without_publishing_and_cleanup_is_owned() {
        let dir = tempfile::tempdir().unwrap();
        let (m, id, temp, cp) = prepare(dir.path(), 16).await;
        let token = CancellationToken::new();
        token.cancel();
        let e = fetch_part(
            &m,
            "fixture",
            "http://127.0.0.1:1",
            &temp,
            0,
            16,
            cp.etag.clone(),
            token,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "ABORTED");
        assert!(temp.exists());
        let mut t = m.task(&id).unwrap();
        let mut forged = cp.clone();
        forged.temp = dir.path().join("user.txt").to_string_lossy().into_owned();
        fs::write(&forged.temp, b"keep").await.unwrap();
        t.checkpoint = serde_json::to_value(forged.clone()).unwrap();
        assert_eq!(cleanup(&t).await.unwrap_err().code, "CONFLICT");
        assert_eq!(fs::read(&forged.temp).await.unwrap(), b"keep");
        cleanup(&m.task(&id).unwrap()).await.unwrap();
        assert!(!temp.exists());
    }
}
