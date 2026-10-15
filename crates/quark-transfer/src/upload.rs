use crate::{
    manager::{cancellable, check, err, io_err, net_err, now, Result, TransferManager},
    protocol::{complete_meta, oss_url, put_meta_with_ctx, xml, OSS_USER_AGENT},
    sha1_state::Sha1State,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{stream::FuturesUnordered, StreamExt};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Default, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    size: u64,
    #[serde(default)]
    modified: u128,
    #[serde(default)]
    md5: String,
    #[serde(default)]
    sha1: String,
    #[serde(default)]
    baseline: Option<Vec<Value>>,
    #[serde(default)]
    pre: Option<Value>,
    #[serde(default)]
    part_size: u64,
    #[serde(default)]
    parts: BTreeMap<u64, String>,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    parallel_requested: bool,
    #[serde(default)]
    parallel_disabled: bool,
    #[serde(default)]
    part_thread: u64,
}
fn store(m: &TransferManager, id: &str, cp: &Checkpoint) -> Result<()> {
    m.checkpoint(
        id,
        serde_json::to_value(cp).map_err(|_| err("LOCAL_IO", "上传进度序列化失败"))?,
    )
}
fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err("SERVER", format!("上传信息缺少 {key}")))
}
#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: u128,
    identity: Option<String>,
}
impl FileStamp {
    fn from(metadata: &std::fs::Metadata) -> Result<Self> {
        if !metadata.is_file() {
            return Err(err("INVALID_ARG", "暂不支持上传文件夹"));
        }
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            Some(format!("{}:{}", metadata.dev(), metadata.ino()))
        };
        #[cfg(not(unix))]
        let identity = None;
        Ok(Self {
            size: metadata.len(),
            modified: metadata
                .modified()
                .map_err(io_err)?
                .duration_since(UNIX_EPOCH)
                .map_err(|_| err("LOCAL_IO", "上传文件修改时间无效"))?
                .as_nanos(),
            identity,
        })
    }
}
struct Inspection {
    md5: String,
    sha1: String,
    contexts: BTreeMap<u64, String>,
    part_md5: BTreeMap<u64, String>,
}
async fn unchanged(path: &Path, expected: &FileStamp) -> Result<()> {
    if FileStamp::from(&fs::metadata(path).await.map_err(io_err)?)? != *expected {
        return Err(err("CONFLICT", "本地文件在上传期间发生变化，已停止提交"));
    }
    Ok(())
}
async fn inspect(
    path: &Path,
    expected: &FileStamp,
    part_size: Option<u64>,
    token: &CancellationToken,
) -> Result<Inspection> {
    let mut file = fs::File::open(path).await.map_err(io_err)?;
    if FileStamp::from(&file.metadata().await.map_err(io_err)?)? != *expected {
        return Err(err("CONFLICT", "本地文件已被替换，已停止上传"));
    }
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut prefix = Sha1State::new();
    let mut part_md5 = Md5::new();
    let mut result = Inspection {
        md5: String::new(),
        sha1: String::new(),
        contexts: BTreeMap::new(),
        part_md5: BTreeMap::new(),
    };
    let mut buffer = vec![0; 1024 * 1024];
    let mut position = 0u64;
    loop {
        check(token)?;
        if let Some(span) = part_size {
            if position > 0 && position < expected.size && position % span == 0 {
                result
                    .contexts
                    .insert(position / span + 1, prefix.export_base64()?);
            }
        }
        let max = part_size
            .map(|span| (span - position % span).min(buffer.len() as u64) as usize)
            .unwrap_or(buffer.len());
        let count = cancellable(token, async {
            file.read(&mut buffer[..max]).await.map_err(io_err)
        })
        .await?;
        if count == 0 {
            break;
        }
        md5.update(&buffer[..count]);
        sha1.update(&buffer[..count]);
        if let Some(span) = part_size {
            prefix.update(&buffer[..count])?;
            part_md5.update(&buffer[..count]);
            position += count as u64;
            if position % span == 0 || position == expected.size {
                result.part_md5.insert(
                    (position - 1) / span + 1,
                    format!("{:x}", part_md5.finalize_reset()),
                );
            }
        } else {
            position += count as u64;
        }
        if position > expected.size {
            return Err(err("CONFLICT", "上传文件读取期间增长，已停止提交"));
        }
    }
    if position != expected.size {
        return Err(err("CONTENT_MISMATCH", "上传源文件读取长度不符"));
    }
    unchanged(path, expected).await?;
    result.md5 = format!("{:x}", md5.finalize());
    result.sha1 = format!("{:x}", sha1.finalize());
    Ok(result)
}
async fn signature(
    m: &TransferManager,
    pre: &Value,
    meta: String,
    token: &CancellationToken,
    generation: u64,
) -> Result<String> {
    let env=cancellable(token,m.inner.core.post_in_session(generation,"/1/clouddrive/file/upload/auth",json!({"task_id":required(pre,"task_id")?,"auth_info":pre["auth_info"],"auth_meta":meta}))).await?;
    required(&env["data"], "auth_key").map(str::to_string)
}
pub async fn run(m: &TransferManager, id: &str, token: &CancellationToken) -> Result<()> {
    let generation = m.inner.core.session_generation();
    let task = m.task(id)?;
    let local = PathBuf::from(
        task.local_path
            .as_deref()
            .ok_or_else(|| err("INVALID_ARG", "上传文件路径缺失"))?,
    );
    let parent = task.pdir_fid.as_deref().unwrap_or("0");
    let name = local
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| err("INVALID_ARG", "文件名不是有效 UTF-8"))?;
    let mut cp: Checkpoint = if task.checkpoint.as_object().is_some_and(|o| o.is_empty()) {
        Checkpoint::default()
    } else {
        serde_json::from_value(task.checkpoint.clone())
            .map_err(|_| err("LOCAL_IO", "上传进度格式无效，原记录已保留"))?
    };
    if (!cp.md5.is_empty()
        && (cp.md5.len() != 32
            || !cp.md5.bytes().all(|b| b.is_ascii_hexdigit())
            || cp.sha1.len() != 40
            || !cp.sha1.bytes().all(|b| b.is_ascii_hexdigit())))
        || (cp.pre.is_some() && (cp.phase.is_empty() || cp.md5.is_empty() || cp.baseline.is_none()))
        || (cp.pre.is_none() && ["prepared", "parts", "completed"].contains(&cp.phase.as_str()))
    {
        return Err(err("LOCAL_IO", "上传进度缺少内容绑定信息，原记录已保留"));
    }
    if cp.phase == "result" {
        return confirm(m, parent, name, &cp, false, token, generation).await;
    }
    if [
        "pre_started",
        "hash_started",
        "complete_started",
        "finish_started",
    ]
    .contains(&cp.phase.as_str())
    {
        // A crash or lost HTTP response at a mutating endpoint cannot safely be replayed.
        // First recognize an attributable completed cloud file; otherwise preserve the checkpoint for explicit inspection.
        if confirm(m, parent, name, &cp, true, token, generation)
            .await
            .is_ok()
        {
            cp.phase = "result".into();
            store(m, id, &cp)?;
            return m.progress(id, cp.size, cp.size);
        }
        check(token)?;
        m.inner.core.ensure_session(generation)?;
        return Err(err(
            "COMMIT_UNCERTAIN",
            "上次上传提交响应未确认，已保留进度；请核对目标目录后重新创建任务，避免重复上传",
        ));
    }
    if !["", "prepared", "parts", "completed"].contains(&cp.phase.as_str()) {
        return Err(err("LOCAL_IO", "上传进度阶段无效，原记录已保留"));
    }
    // Once OSS completion is acknowledged, finishing that same task does not
    // need the local file to remain available. Its digest is already pinned.
    if cp.phase == "completed" {
        let pre = cp
            .pre
            .clone()
            .ok_or_else(|| err("LOCAL_IO", "上传进度缺少预登记信息"))?;
        let finish =
            json!({"obj_key":required(&pre,"obj_key")?,"task_id":required(&pre,"task_id")?});
        check(token)?;
        m.inner.core.ensure_session(generation)?;
        cp.phase = "finish_started".into();
        store(m, id, &cp)?;
        m.inner
            .core
            .post_in_session(generation, "/1/clouddrive/file/upload/finish", finish)
            .await?;
        cp.phase = "result".into();
        store(m, id, &cp)?;
        return confirm(m, parent, name, &cp, false, token, generation).await;
    }
    let stamp = FileStamp::from(&fs::metadata(&local).await.map_err(io_err)?)?;
    let size = stamp.size;
    if (!cp.md5.is_empty() || cp.pre.is_some())
        && (cp.size != size
            || cp.modified != stamp.modified
            || cp
                .identity
                .as_ref()
                .is_some_and(|old| stamp.identity.as_ref() != Some(old)))
    {
        return Err(err(
            "CONFLICT",
            "本地文件在上传期间发生变化，请核对旧上传结果并重新创建任务",
        ));
    }
    cp.size = size;
    cp.modified = stamp.modified;
    cp.identity = stamp.identity.clone();
    m.progress(id, 0, size)?;
    if cp.baseline.is_none() {
        cp.baseline = Some(matching(m, parent, name, token, generation).await?);
        store(m, id, &cp)?;
    }
    // Rehash on resume, including when a process changed bytes and restored the
    // mtime. Never mix saved multipart ETags with a different local source.
    let inspected = inspect(&local, &stamp, None, token).await?;
    if cp.md5.is_empty() {
        cp.md5 = inspected.md5;
        cp.sha1 = inspected.sha1;
        store(m, id, &cp)?;
    } else if cp.md5 != inspected.md5 || cp.sha1 != inspected.sha1 {
        return Err(err(
            "CONFLICT",
            "本地文件内容已变化，已保留旧上传进度且不会重复建档",
        ));
    }
    let mime = mime_guess::from_path(&local)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    if cp.pre.is_none() {
        check(token)?;
        cp.parallel_requested = m.inner.core.settings()["uploadPartConcurrency"]
            .as_u64()
            .unwrap_or(3)
            > 1;
        cp.phase = "pre_started".into();
        store(m, id, &cp)?;
        // Do not cancel an in-flight pre request: wait for its response so an accepted upload cannot be forgotten.
        let result=m.inner.core.post_in_session(generation,"/1/clouddrive/file/upload/pre",json!({"ccp_hash_update":true,"parallel_upload":cp.parallel_requested,"dir_name":"","file_name":name,"format_type":mime,"l_created_at":now(),"l_updated_at":now(),"pdir_fid":parent,"size":size})).await;
        let env = match result {
            Ok(env) => env,
            Err(e) => {
                if e.code == "NEED_LOGIN" {
                    cp.phase.clear();
                    store(m, id, &cp)?;
                }
                return Err(e);
            }
        };
        cp.part_size = env["metadata"]["part_size"]
            .as_u64()
            .or_else(|| env["data"]["part_size"].as_u64())
            .unwrap_or(0);
        cp.part_thread = env["metadata"]["part_thread"]
            .as_u64()
            .or_else(|| env["data"]["part_thread"].as_u64())
            .unwrap_or(3)
            .clamp(1, 8);
        cp.pre = Some(env["data"].clone());
        cp.phase = "prepared".into();
        store(m, id, &cp)?;
    }
    let pre = cp.pre.clone().unwrap();
    if pre["finish"].as_bool() == Some(true) {
        cp.phase = "result".into();
        store(m, id, &cp)?;
        m.progress(id, size, size)?;
        return confirm(m, parent, name, &cp, false, token, generation).await;
    }
    check(token)?;
    if cp.phase == "prepared" {
        let hash_request = json!({"md5":cp.md5,"sha1":cp.sha1,"task_id":required(&pre,"task_id")?});
        check(token)?;
        cp.phase = "hash_started".into();
        store(m, id, &cp)?;
        let hash_result = m
            .inner
            .core
            .post_in_session(generation, "/1/clouddrive/file/update/hash", hash_request)
            .await;
        match hash_result {
            Ok(env) if env["data"]["finish"].as_bool() == Some(true) => {
                cp.phase = "result".into();
                store(m, id, &cp)?;
                m.progress(id, size, size)?;
                return confirm(m, parent, name, &cp, false, token, generation).await;
            }
            Ok(_) => {}
            Err(e) => {
                // A lost hash-update response may already have finalized an
                // instant upload. Preserve ambiguity instead of sending PUTs.
                if e.code == "NEED_LOGIN" {
                    cp.phase = "prepared".into();
                    store(m, id, &cp)?;
                }
                return Err(e);
            }
        }
        cp.phase = "parts".into();
        store(m, id, &cp)?;
    }
    let key = required(&pre, "obj_key")?;
    let task_id = required(&pre, "task_id")?;
    if size == 0 {
        if cp.phase != "result" {
            check(token)?;
            cp.phase = "finish_started".into();
            store(m, id, &cp)?;
            m.inner
                .core
                .post_in_session(
                    generation,
                    "/1/clouddrive/file/upload/finish",
                    json!({"obj_key":key,"task_id":task_id}),
                )
                .await?;
            cp.phase = "result".into();
            store(m, id, &cp)?;
        }
        return confirm(m, parent, name, &cp, false, token, generation).await;
    }
    let part_size = cp.part_size;
    if part_size == 0 || part_size % 65536 != 0 || part_size > 16 * 1024 * 1024 {
        return Err(err(
            "SERVER",
            "服务端分片大小不合法，必须为 65536 倍数且不超过 16 MiB",
        ));
    }
    let bucket = required(&pre, "bucket")?;
    let upload_id = required(&pre, "upload_id")?;
    let base = oss_url(required(&pre, "upload_url")?, bucket, key)?;
    if cp.phase == "parts" {
        let count = size.div_ceil(part_size);
        if count > 10_000 {
            return Err(err("INVALID_ARG", "服务端分片参数会超过 10000 片上限"));
        }
        let inspected = inspect(&local, &stamp, Some(part_size), token).await?;
        if inspected.md5 != cp.md5 || inspected.sha1 != cp.sha1 {
            return Err(err("CONFLICT", "上传源文件内容已变化，已停止分片提交"));
        }
        for (number, tag) in &cp.parts {
            if !inspected
                .part_md5
                .get(number)
                .is_some_and(|expected| expected.eq_ignore_ascii_case(tag))
            {
                return Err(err(
                    "CONTENT_MISMATCH",
                    "已保存上传分片与本地内容不一致，已保留记录",
                ));
            }
        }
        loop {
            let concurrency = if cp.parallel_requested && !cp.parallel_disabled {
                m.inner.core.settings()["uploadPartConcurrency"]
                    .as_u64()
                    .unwrap_or(3)
                    .clamp(1, 8)
                    .min(cp.part_thread.max(1))
                    .min((32 * 1024 * 1024 / part_size).max(1)) as usize
            } else {
                1
            };
            let contexts = &inspected.contexts;
            let part_hashes = &inspected.part_md5;
            let with_context = cp.parallel_requested && !cp.parallel_disabled;
            let local_ref = &local;
            let stamp_ref = &stamp;
            let pre_ref = &pre;
            let base_ref = &base;
            let mime_ref = mime.as_str();
            let launch = |number| async move {
                let _buffer = cancellable(token, async {
                    m.inner
                        .upload_buffers
                        .acquire_many(part_size.div_ceil(1024) as u32)
                        .await
                        .map_err(|_| err("ABORTED", "上传已停止"))
                })
                .await?;
                let bytes = read_part(
                    local_ref,
                    stamp_ref,
                    part_size,
                    number,
                    part_hashes
                        .get(&number)
                        .ok_or_else(|| err("CONTENT_MISMATCH", "缺少上传分片摘要"))?,
                    token,
                )
                .await?;
                let length = bytes.len() as u64;
                let context = if with_context {
                    contexts.get(&number).map(String::as_str)
                } else {
                    None
                };
                let etag = put_part(
                    m, pre_ref, base_ref, mime_ref, bucket, key, upload_id, number, bytes, context,
                    token, generation,
                )
                .await?;
                Ok::<_, quark_core::Error>((number, length, etag))
            };
            let pending = (1..=count)
                .filter(|n| !cp.parts.contains_key(n))
                .collect::<Vec<_>>();
            let mut uploaded = cp
                .parts
                .keys()
                .map(|n| part_size.min(size - (n - 1) * part_size))
                .sum::<u64>();
            m.progress(id, uploaded, size)?;
            let result = part_window(pending, concurrency, launch, |number, length, etag| {
                uploaded += length;
                cp.parts.insert(number, etag);
                store(m, id, &cp)?;
                m.progress(id, uploaded, size)
            })
            .await;
            match result {
                Ok(()) => break,
                Err(error)
                    if error.code == "PARALLEL_UNSUPPORTED"
                        && cp.parallel_requested
                        && !cp.parallel_disabled =>
                {
                    check(token)?;
                    cp.parallel_disabled = true;
                    // Reuse the exact same multipart upload, overwriting its
                    // parts in ascending order. Never repeat upload/pre here.
                    cp.parts.clear();
                    store(m, id, &cp)?;
                }
                Err(error) => return Err(error),
            }
        }
        if cp.parts.len() as u64 != count {
            return Err(err("CONTENT_MISMATCH", "上传分片未完整提交"));
        }
        let final_source = inspect(&local, &stamp, None, token).await?;
        if final_source.md5 != cp.md5 || final_source.sha1 != cp.sha1 {
            return Err(err("CONFLICT", "本地文件在上传过程中发生变化，已停止提交"));
        }
        let parts: Vec<_> = cp.parts.iter().map(|(n, tag)| (*n, tag.clone())).collect();
        let xml = xml(&parts);
        let digest = STANDARD.encode(Md5::digest(xml.as_bytes()));
        let callback = STANDARD.encode(
            serde_json::to_vec(&pre["callback"]).map_err(|_| err("SERVER", "上传回调格式无效"))?,
        );
        let date = httpdate::fmt_http_date(SystemTime::now());
        let auth = signature(
            m,
            &pre,
            complete_meta(&digest, &date, &callback, bucket, key, upload_id),
            token,
            generation,
        )
        .await?;
        let mut url = base.clone();
        url.query_pairs_mut().append_pair("uploadId", upload_id);
        check(token)?;
        m.inner.core.ensure_session(generation)?;
        cp.phase = "complete_started".into();
        store(m, id, &cp)?;
        let response = m
            .inner
            .core
            .http_client()
            .post(url)
            .header("authorization", auth)
            .header("content-md5", digest)
            .header("content-type", "application/xml")
            .header("x-oss-callback", callback)
            .header("x-oss-date", date)
            .header("x-oss-user-agent", OSS_USER_AGENT)
            .body(xml)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(net_err)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = small_response(response).await?;
            let code = text
                .split_once("<Code>")
                .and_then(|(_, s)| s.split_once("</Code>"))
                .map(|(s, _)| s)
                .filter(|s| s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
                .unwrap_or("Rejected");
            return Err(err(
                "SERVER",
                format!("对象存储完成上传返回 HTTP {status} ({code})"),
            ));
        }
        let text = small_response(response).await?;
        validate_completion(&text)?;
        cp.phase = "completed".into();
        store(m, id, &cp)?;
    }
    if cp.phase == "completed" {
        check(token)?;
        cp.phase = "finish_started".into();
        store(m, id, &cp)?;
        m.inner
            .core
            .post_in_session(
                generation,
                "/1/clouddrive/file/upload/finish",
                json!({"obj_key":key,"task_id":task_id}),
            )
            .await?;
        cp.phase = "result".into();
        store(m, id, &cp)?;
    }
    m.progress(id, size, size)?;
    confirm(m, parent, name, &cp, false, token, generation).await
}
async fn part_window<F, Fut, S>(
    pending: Vec<u64>,
    concurrency: usize,
    launch: F,
    mut save: S,
) -> Result<()>
where
    F: Fn(u64) -> Fut,
    Fut: std::future::Future<Output = Result<(u64, u64, String)>>,
    S: FnMut(u64, u64, String) -> Result<()>,
{
    if concurrency == 0 {
        return Err(err("INVALID_ARG", "上传并发数必须大于零"));
    }
    let mut pending = pending.into_iter();
    let mut work = FuturesUnordered::new();
    for number in pending.by_ref().take(concurrency) {
        work.push(launch(number));
    }
    let mut failure = None;
    while let Some(result) = work.next().await {
        match result {
            Ok((number, length, etag)) => {
                if let Err(error) = save(number, length, etag) {
                    failure = Some(error);
                }
            }
            Err(error) => {
                if failure.is_none() || error.code != "PARALLEL_UNSUPPORTED" {
                    failure = Some(error);
                }
            }
        }
        // Stop issuing new PUTs on first failure, then drain and checkpoint
        // already in-flight successes before a caller may retry sequentially.
        if failure.is_none() {
            if let Some(number) = pending.next() {
                work.push(launch(number));
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

async fn read_part(
    path: &Path,
    stamp: &FileStamp,
    part_size: u64,
    number: u64,
    expected_md5: &str,
    token: &CancellationToken,
) -> Result<bytes::Bytes> {
    check(token)?;
    unchanged(path, stamp).await?;
    let start = number
        .checked_sub(1)
        .and_then(|n| n.checked_mul(part_size))
        .filter(|start| *start < stamp.size)
        .ok_or_else(|| err("CONTENT_MISMATCH", "上传分片范围无效"))?;
    let mut file = fs::File::open(path).await.map_err(io_err)?;
    if FileStamp::from(&file.metadata().await.map_err(io_err)?)? != *stamp {
        return Err(err("CONFLICT", "本地上传文件已被替换"));
    }
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(io_err)?;
    let mut bytes = vec![0; part_size.min(stamp.size - start) as usize];
    cancellable(token, async {
        file.read_exact(&mut bytes).await.map_err(io_err)?;
        Ok(())
    })
    .await?;
    if !format!("{:x}", Md5::digest(&bytes)).eq_ignore_ascii_case(expected_md5) {
        return Err(err("CONFLICT", "本地上传分片内容已变化，已停止传输"));
    }
    unchanged(path, stamp).await?;
    Ok(bytes.into())
}

async fn small_response(mut response: reqwest::Response) -> Result<String> {
    const MAX: usize = 64 * 1024;
    if response.content_length().is_some_and(|n| n > MAX as u64) {
        return Err(err("SERVER", "对象存储响应过大"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(net_err)? {
        if body.len().saturating_add(chunk.len()) > MAX {
            return Err(err("SERVER", "对象存储响应过大"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| err("SERVER", "对象存储响应编码无效"))
}

fn validate_completion(text: &str) -> Result<()> {
    let lower = text.trim().to_ascii_lowercase();
    if lower.contains("<error") || lower.is_empty() {
        return Err(err("SERVER", "对象存储没有确认完成上传"));
    }
    if lower.contains("<completemultipartuploadresult") {
        return Ok(());
    }
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if value.as_object().is_some_and(|o| !o.is_empty())
            && value["success"] != false
            && !value["code"]
                .as_i64()
                .is_some_and(|code| code != 0 && code != 200)
            && !value["status"].as_str().is_some_and(|s| {
                ["error", "failed", "fail"].contains(&s.to_ascii_lowercase().as_str())
            })
        {
            return Ok(());
        }
    }
    Err(err(
        "SERVER",
        "对象存储完成响应未通过检查，已保留待确认进度",
    ))
}

async fn put_part(
    m: &TransferManager,
    pre: &Value,
    base: &reqwest::Url,
    mime: &str,
    bucket: &str,
    key: &str,
    upload_id: &str,
    number: u64,
    bytes: bytes::Bytes,
    hash_context: Option<&str>,
    token: &CancellationToken,
    generation: u64,
) -> Result<String> {
    for attempt in 0..3 {
        check(token)?;
        let date = httpdate::fmt_http_date(SystemTime::now());
        let auth = signature(
            m,
            pre,
            put_meta_with_ctx(mime, &date, bucket, key, number, upload_id, hash_context),
            token,
            generation,
        )
        .await?;
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("partNumber", &number.to_string())
            .append_pair("uploadId", upload_id);
        let result = cancellable(
            token,
            put_request(
                &m.inner.core.http_client(),
                url,
                mime,
                &date,
                &auth,
                bytes.clone(),
                hash_context,
            ),
        )
        .await;
        match result {
            Ok(etag) => return Ok(etag),
            Err(e)
                if attempt < 2 && ["NETWORK", "SIGNATURE_EXPIRED"].contains(&e.code.as_str()) =>
            {
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

async fn put_request(
    client: &reqwest::Client,
    url: reqwest::Url,
    mime: &str,
    date: &str,
    auth: &str,
    bytes: bytes::Bytes,
    hash_context: Option<&str>,
) -> Result<String> {
    let expected = format!("{:x}", Md5::digest(&bytes));
    let mut request = client
        .put(url)
        .header("authorization", auth)
        .header("content-type", mime)
        .header("content-length", bytes.len())
        .header("x-oss-date", date)
        .header("x-oss-user-agent", OSS_USER_AGENT)
        .body(bytes)
        .timeout(Duration::from_secs(180));
    if let Some(context) = hash_context {
        request = request.header("X-Oss-Hash-Ctx", context);
    }
    // This client has no cookie store; cloud-account cookies are never sent to OSS.
    let response = request.send().await.map_err(net_err)?;
    let status = response.status();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim_matches('"').to_string());
    let text = small_response(response).await?;
    if !status.is_success() {
        let lower = text.to_ascii_lowercase();
        let protocol_rejection = status.as_u16() == 400
            && (hash_context.is_some()
                || [
                    "partnotsequential",
                    "contextcomparefailed",
                    "invalidpartorder",
                ]
                .iter()
                .any(|s| lower.contains(s)));
        return Err(err(
            if protocol_rejection {
                "PARALLEL_UNSUPPORTED"
            } else if status.as_u16() == 403 {
                "SIGNATURE_EXPIRED"
            } else if status.is_server_error() {
                "NETWORK"
            } else {
                "SERVER"
            },
            format!("上传分片返回 HTTP {}", status.as_u16()),
        ));
    }
    if text.to_ascii_lowercase().contains("<error") {
        return Err(err("SERVER", "对象存储未接受上传分片"));
    }
    let etag = etag.ok_or_else(|| err("SERVER", "上传分片未返回 ETag"))?;
    if etag.len() != 32
        || !etag.bytes().all(|b| b.is_ascii_hexdigit())
        || !etag.eq_ignore_ascii_case(&expected)
    {
        return Err(err(
            "CONTENT_MISMATCH",
            "上传分片 ETag 与本地 MD5 不一致，已停止完成上传",
        ));
    }
    Ok(etag)
}
async fn matching(
    m: &TransferManager,
    parent: &str,
    name: &str,
    token: &CancellationToken,
    generation: u64,
) -> Result<Vec<Value>> {
    use unicode_normalization::UnicodeNormalization;
    let normalized = name.nfc().collect::<String>();
    let mut matches = Vec::new();
    for page in 1..=1000 {
        m.inner.core.ensure_session(generation)?;
        let result = cancellable(
            token,
            m.inner
                .core
                .call("fs.list", json!({"pdirFid":parent,"page":page,"size":100})),
        )
        .await?;
        m.inner.core.ensure_session(generation)?;
        let list = result["list"]
            .as_array()
            .ok_or_else(|| err("SERVER", "目录列表格式无效"))?;
        for item in list {
            if item["dir"].as_bool() != Some(true)
                && item["name"]
                    .as_str()
                    .is_some_and(|s| s.nfc().collect::<String>() == normalized)
                && !matches.iter().any(|old: &Value| old["fid"] == item["fid"])
            {
                matches.push(item.clone());
            }
        }
        if list.is_empty() || result["hasMore"].as_bool() != Some(true) {
            return Ok(matches);
        }
    }
    Err(err("TASK_FAILED", "目标目录过大，无法确认上传归属"))
}
async fn confirm(
    m: &TransferManager,
    parent: &str,
    name: &str,
    checkpoint: &Checkpoint,
    require_digest: bool,
    token: &CancellationToken,
    generation: u64,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        check(token)?;
        let files = tokio::time::timeout_at(deadline, matching(m, parent, name, token, generation))
            .await
            .map_err(|_| {
                err(
                    "TASK_FAILED",
                    "确认上传结果超时；重试只核对结果，不重复上传",
                )
            })??;
        let changed: Vec<_> = files
            .iter()
            .filter(|item| {
                item["size"].as_u64() == Some(checkpoint.size)
                    && checkpoint
                        .baseline
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .find(|old| old["fid"] == item["fid"])
                        .is_none_or(|old| {
                            old["size"] != item["size"]
                                || item["updatedAt"].as_u64().unwrap_or(0)
                                    > old["updatedAt"].as_u64().unwrap_or(0)
                        })
            })
            .collect();
        if changed.len() == 1 {
            if verified_result(changed[0], checkpoint, require_digest)? {
                return Ok(());
            }
        }
        if changed.len() > 1 {
            return Err(err("CONFLICT", "出现多个同名上传结果，请核对归属"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(err(
                "TASK_FAILED",
                "服务端已接收上传，但尚未确认文件大小；重试只核对结果，不重复上传",
            ));
        }
        cancellable(token, async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(())
        })
        .await?;
    }
}

fn verified_result(item: &Value, checkpoint: &Checkpoint, require_digest: bool) -> Result<bool> {
    let mut verified = false;
    for (field, expected) in [("md5", &checkpoint.md5), ("sha1", &checkpoint.sha1)] {
        if let Some(actual) = item[field].as_str().filter(|s| !s.is_empty()) {
            if expected.is_empty() || !actual.eq_ignore_ascii_case(expected) {
                return Err(err(
                    "CONTENT_MISMATCH",
                    "云端上传结果摘要与本地文件不一致，已保留待核对记录",
                ));
            }
            verified = true;
        }
    }
    Ok(verified || !require_digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[tokio::test]
    async fn prefix_inspection_and_saved_part_replay_bind_to_actual_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        let bytes: Vec<_> = (0..131_083).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let stamp = FileStamp::from(&std::fs::metadata(&path).unwrap()).unwrap();
        let token = CancellationToken::new();
        let result = inspect(&path, &stamp, Some(65536), &token).await.unwrap();
        assert_eq!(result.md5, format!("{:x}", Md5::digest(&bytes)));
        assert_eq!(result.sha1, format!("{:x}", Sha1::digest(&bytes)));
        assert_eq!(result.contexts.keys().copied().collect::<Vec<_>>(), [2, 3]);
        let decoded: Value =
            serde_json::from_slice(&STANDARD.decode(&result.contexts[&3]).unwrap()).unwrap();
        assert_eq!(decoded["Nl"], (131072u64 * 8).to_string());
        assert_eq!(
            read_part(&path, &stamp, 65536, 3, &result.part_md5[&3], &token)
                .await
                .unwrap()
                .as_ref(),
            &bytes[131072..]
        );
        let original_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut changed = bytes;
        changed[0] ^= 1;
        std::fs::write(&path, &changed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        assert_eq!(
            FileStamp::from(&std::fs::metadata(&path).unwrap())
                .unwrap()
                .modified,
            stamp.modified
        );
        assert_eq!(
            read_part(&path, &stamp, 65536, 1, &result.part_md5[&1], &token)
                .await
                .unwrap_err()
                .code,
            "CONFLICT"
        );
        token.cancel();
        assert_eq!(
            inspect(&path, &stamp, Some(65536), &token)
                .await
                .err()
                .unwrap()
                .code,
            "ABORTED"
        );
    }

    fn fixture(
        status: &str,
        body: &str,
        etag: Option<&str>,
    ) -> (reqwest::Url, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = reqwest::Url::parse(&format!("http://{}/part", listener.local_addr().unwrap()))
            .unwrap();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{body}",
            body.len(),
            etag.map(|s| format!("ETag: \"{s}\"\r\n"))
                .unwrap_or_default()
        );
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let size = socket.read(&mut buffer).unwrap();
                if size == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..size]);
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let length = header
                        .lines()
                        .find_map(|s| s.strip_prefix("content-length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            socket.write_all(reply.as_bytes()).unwrap();
            bytes
        });
        (url, handle)
    }

    #[tokio::test]
    async fn actual_http_put_carries_prefix_without_cookies_and_rejects_bad_etag() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let payload = bytes::Bytes::from_static(b"fixture-upload-bytes");
        let md5 = format!("{:x}", Md5::digest(&payload));
        let (url, handle) = fixture("200 OK", "", Some(&md5));
        assert_eq!(
            put_request(
                &client,
                url,
                "application/octet-stream",
                "date",
                "fixture-auth",
                payload.clone(),
                Some("Y3R4")
            )
            .await
            .unwrap(),
            md5
        );
        let request = String::from_utf8(handle.join().unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(request.contains("x-oss-hash-ctx: y3r4\r\n"));
        assert!(!request.contains("cookie:"));
        assert!(request.ends_with("fixture-upload-bytes"));
        let (url, handle) = fixture("200 OK", "", Some(&md5.to_ascii_uppercase()));
        assert_eq!(
            put_request(
                &client,
                url,
                "application/octet-stream",
                "date",
                "fixture-auth",
                payload.clone(),
                None
            )
            .await
            .unwrap(),
            md5.to_ascii_uppercase()
        );
        handle.join().unwrap();
        let (url, handle) = fixture("200 OK", "", Some("00000000000000000000000000000000"));
        assert_eq!(
            put_request(
                &client,
                url,
                "application/octet-stream",
                "date",
                "fixture-auth",
                payload.clone(),
                None
            )
            .await
            .unwrap_err()
            .code,
            "CONTENT_MISMATCH"
        );
        handle.join().unwrap();
        let (url, handle) = fixture(
            "400 Bad Request",
            "<Error><Code>ContextCompareFailed</Code></Error>",
            None,
        );
        assert_eq!(
            put_request(
                &client,
                url,
                "application/octet-stream",
                "date",
                "fixture-auth",
                payload,
                Some("Y3R4")
            )
            .await
            .unwrap_err()
            .code,
            "PARALLEL_UNSUPPORTED"
        );
        handle.join().unwrap();
    }

    #[test]
    fn completion_errors_and_cloud_digest_mismatches_cannot_finish_a_task() {
        assert!(validate_completion(
            "<CompleteMultipartUploadResult><ETag>a</ETag></CompleteMultipartUploadResult>"
        )
        .is_ok());
        assert!(validate_completion(r#"{"code":0,"data":{}}"#).is_ok());
        for value in [
            "",
            "<Error><Code>InvalidPart</Code></Error>",
            r#"{"code":500}"#,
            r#"{"success":false}"#,
            "<html>bad gateway</html>",
        ] {
            assert!(validate_completion(value).is_err(), "{value}");
        }
        let cp = Checkpoint {
            md5: "aa".into(),
            sha1: "bb".into(),
            ..Checkpoint::default()
        };
        assert!(verified_result(&json!({"md5":"aa"}), &cp, true).unwrap());
        assert!(!verified_result(&json!({}), &cp, true).unwrap());
        assert!(verified_result(&json!({}), &cp, false).unwrap());
        assert_eq!(
            verified_result(&json!({"md5":"cc"}), &cp, false)
                .unwrap_err()
                .code,
            "CONTENT_MISMATCH"
        );
    }
}
