use crate::{
    manager::{cancellable, check, err, io_err, net_err, Result, Task, TransferManager},
    protocol::{content_range, safe_name},
};
use futures_util::{stream::FuturesUnordered, StreamExt};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha1::Sha1;
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
const PART: u64 = 8 * 1024 * 1024;
#[derive(Clone, Default, Serialize, Deserialize)]
struct DonePart {
    len: u64,
    sha1: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Checkpoint {
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
    if !cp.temp.is_empty() && cp.total != total {
        return Err(err("CONFLICT", "云端文件大小已改变，请取消并重新下载"));
    }
    if cp.target.is_empty() {
        let dir = PathBuf::from(task.local_dir.as_deref().unwrap_or(""));
        fs::create_dir_all(&dir).await.map_err(io_err)?;
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
            if existing != (cp.md5.clone(), cp.sha1.clone()) {
                return Err(err("CONFLICT", "下载目标已被其他文件占用"));
            }
            let _ = fs::remove_file(&temp).await;
            let _ = fs::remove_file(format!("{}.qkreserve", cp.target)).await;
            return m.progress(id, total, total);
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
    for (index, done) in cp.parts.clone() {
        let start = index
            .checked_mul(PART)
            .ok_or_else(|| err("LOCAL_IO", "下载分片位置溢出"))?;
        let len = PART.min(total.saturating_sub(start));
        if start >= total
            || done.len != len
            || hash_range(&temp, start, len, token).await? != done.sha1
        {
            cp.parts.remove(&index);
        }
    }
    store(m, id, &cp)?;
    let mut link = get_link(m, &fid, token).await?;
    let mut probe = send(m, &link, Some((0, 0)), token).await?;
    if probe.status() == reqwest::StatusCode::FORBIDDEN {
        link = get_link(m, &fid, token).await?;
        probe = send(m, &link, Some((0, 0)), token).await?;
    }
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
    if !cp.etag.is_empty() && !etag.is_empty() && etag != cp.etag {
        cp.parts.clear();
    }
    cp.etag = etag;
    drop(probe);
    store(m, id, &cp)?;
    let result = if range && total > 0 {
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
    let (md5, sha1) = hash_file(&temp, token).await?;
    for (key, value) in [("md5", &md5), ("sha1", &sha1)] {
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
    store(m, id, &cp)?;
    publish(&temp, Path::new(&cp.target), token).await?;
    task.local_path = Some(cp.target.clone());
    m.progress(id, total, total)
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
async fn send(
    m: &TransferManager,
    url: &str,
    range: Option<(u64, u64)>,
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
            req = req.header("cookie", m.inner.core.cookie_header());
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
    if status == reqwest::StatusCode::FORBIDDEN {
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
    let parallel = settings["downloadParallel"]
        .as_u64()
        .unwrap_or(4)
        .clamp(1, 8) as usize;
    let pending: Vec<_> = (0..count).filter(|i| !cp.parts.contains_key(i)).collect();
    let mut cursor = 0;
    let mut workers = FuturesUnordered::new();
    let workers_token = token.child_token();
    let mut failure = None;
    let mut complete = cp.parts.values().map(|p| p.len).sum::<u64>();
    m.progress(id, complete, cp.total)?;
    let live = Arc::new(Mutex::new((complete, BTreeMap::<u64, u64>::new())));
    loop {
        while failure.is_none() && workers.len() < parallel && cursor < pending.len() {
            let index = pending[cursor];
            cursor += 1;
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
    let start = index * PART;
    let length = PART.min(total - start);
    let mut link = link.to_string();
    for attempt in 0..3 {
        check(&token)?;
        if let Some(progress) = &on_progress {
            progress(0);
        }
        let result = async {
            let response = send(m, &link, Some((start, start + length - 1)), &token).await?;
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
                return Err(err("RANGE_UNSUPPORTED", "分片响应范围与请求不符"));
            }
            if let Some(cl) = response.content_length() {
                if cl != length {
                    return Err(err("CONTENT_MISMATCH", "分片 Content-Length 不符"));
                }
            }
            let current = response
                .headers()
                .get("etag")
                .and_then(|s| s.to_str().ok())
                .unwrap_or("");
            if !etag.is_empty() && !current.is_empty() && etag != current {
                return Err(err("CONFLICT", "下载过程中云端文件已改变"));
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
            let mut hash = Sha1::new();
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
                    sha1: format!("{:x}", hash.finalize()),
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
    let mut link = link.to_string();
    cp.parts.clear();
    store(m, id, cp)?;
    for attempt in 0..3 {
        let result = async {
            let response = send(m, &link, None, token).await?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(http_error(response.status()));
            }
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
    let mut hash = Sha1::new();
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
async fn hash_file(path: &Path, token: &CancellationToken) -> Result<(String, String)> {
    let mut file = fs::File::open(path).await.map_err(io_err)?;
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        check(token)?;
        let n = file.read(&mut buf).await.map_err(io_err)?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha1.update(&buf[..n]);
    }
    Ok((
        format!("{:x}", md5.finalize()),
        format!("{:x}", sha1.finalize()),
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
