use crate::{
    manager::{cancellable, check, err, io_err, net_err, now, Result, TransferManager},
    protocol::{complete_meta, oss_url, put_meta, xml, OSS_USER_AGENT},
};
use base64::{engine::general_purpose::STANDARD, Engine};
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
async fn hashes(path: &Path, token: &CancellationToken) -> Result<(String, String)> {
    let mut file = fs::File::open(path).await.map_err(io_err)?;
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        check(token)?;
        let count = file.read(&mut buffer).await.map_err(io_err)?;
        if count == 0 {
            break;
        }
        md5.update(&buffer[..count]);
        sha1.update(&buffer[..count]);
    }
    Ok((
        format!("{:x}", md5.finalize()),
        format!("{:x}", sha1.finalize()),
    ))
}
async fn signature(
    m: &TransferManager,
    pre: &Value,
    meta: String,
    token: &CancellationToken,
) -> Result<String> {
    let env=cancellable(token,m.inner.core.post("/1/clouddrive/file/upload/auth",json!({"task_id":required(pre,"task_id")?,"auth_info":pre["auth_info"],"auth_meta":meta}))).await?;
    required(&env["data"], "auth_key").map(str::to_string)
}
pub async fn run(m: &TransferManager, id: &str, token: &CancellationToken) -> Result<()> {
    let task = m.task(id)?;
    let local = PathBuf::from(
        task.local_path
            .as_deref()
            .ok_or_else(|| err("INVALID_ARG", "上传文件路径缺失"))?,
    );
    let metadata = fs::metadata(&local).await.map_err(io_err)?;
    if !metadata.is_file() {
        return Err(err("INVALID_ARG", "暂不支持上传文件夹"));
    }
    let size = metadata.len();
    let modified = metadata
        .modified()
        .map_err(io_err)?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
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
    if (!cp.md5.is_empty() || cp.pre.is_some()) && (cp.size != size || cp.modified != modified) {
        return Err(err(
            "CONFLICT",
            "本地文件在上传期间发生变化，请核对旧上传结果并重新创建任务",
        ));
    }
    cp.size = size;
    cp.modified = modified;
    m.progress(id, 0, size)?;
    if cp.baseline.is_none() {
        cp.baseline = Some(matching(m, parent, name, token).await?);
        store(m, id, &cp)?;
    }
    if cp.phase == "result" {
        return confirm(
            m,
            parent,
            name,
            size,
            cp.baseline.as_deref().unwrap_or(&[]),
            token,
        )
        .await;
    }
    if ["pre_started", "complete_started", "finish_started"].contains(&cp.phase.as_str()) {
        // A crash or lost HTTP response at a mutating endpoint cannot safely be replayed.
        // First recognize an attributable completed cloud file; otherwise preserve the checkpoint for explicit inspection.
        if confirm(
            m,
            parent,
            name,
            size,
            cp.baseline.as_deref().unwrap_or(&[]),
            token,
        )
        .await
        .is_ok()
        {
            cp.phase = "result".into();
            store(m, id, &cp)?;
            return m.progress(id, size, size);
        }
        return Err(err(
            "COMMIT_UNCERTAIN",
            "上次上传提交响应未确认，已保留进度；请核对目标目录后重新创建任务，避免重复上传",
        ));
    }
    if cp.md5.is_empty() {
        let (md5, sha1) = hashes(&local, token).await?;
        cp.md5 = md5;
        cp.sha1 = sha1;
        store(m, id, &cp)?;
    }
    let mime = mime_guess::from_path(&local)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    if cp.pre.is_none() {
        cp.phase = "pre_started".into();
        store(m, id, &cp)?;
        // Do not cancel an in-flight pre request: wait for its response so an accepted upload cannot be forgotten.
        check(token)?;
        let result=m.inner.core.post("/1/clouddrive/file/upload/pre",json!({"ccp_hash_update":true,"dir_name":"","file_name":name,"format_type":mime,"l_created_at":now(),"l_updated_at":now(),"pdir_fid":parent,"size":size})).await;
        let env = match result {
            Ok(env) => env,
            Err(e) => {
                if !["NETWORK", "TIMEOUT"].contains(&e.code.as_str()) {
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
        cp.pre = Some(env["data"].clone());
        cp.phase = "prepared".into();
        store(m, id, &cp)?;
    }
    let pre = cp.pre.clone().unwrap();
    if pre["finish"].as_bool() == Some(true) {
        cp.phase = "result".into();
        store(m, id, &cp)?;
        m.progress(id, size, size)?;
        return confirm(
            m,
            parent,
            name,
            size,
            cp.baseline.as_deref().unwrap_or(&[]),
            token,
        )
        .await;
    }
    check(token)?;
    if cp.phase == "prepared" {
        let hash_result = m
            .inner
            .core
            .post(
                "/1/clouddrive/file/update/hash",
                json!({"md5":cp.md5,"sha1":cp.sha1,"task_id":required(&pre,"task_id")?}),
            )
            .await;
        match hash_result {
            Ok(env) if env["data"]["finish"].as_bool() == Some(true) => {
                cp.phase = "result".into();
                store(m, id, &cp)?;
                m.progress(id, size, size)?;
                return confirm(
                    m,
                    parent,
                    name,
                    size,
                    cp.baseline.as_deref().unwrap_or(&[]),
                    token,
                )
                .await;
            }
            Err(e) if e.code == "NEED_LOGIN" => return Err(e),
            _ => {}
        }
        cp.phase = "parts".into();
        store(m, id, &cp)?;
    }
    let key = required(&pre, "obj_key")?;
    let task_id = required(&pre, "task_id")?;
    if size == 0 {
        if cp.phase != "finished" {
            cp.phase = "finish_started".into();
            store(m, id, &cp)?;
            m.inner
                .core
                .post(
                    "/1/clouddrive/file/upload/finish",
                    json!({"obj_key":key,"task_id":task_id}),
                )
                .await?;
            cp.phase = "result".into();
            store(m, id, &cp)?;
        }
        return confirm(
            m,
            parent,
            name,
            size,
            cp.baseline.as_deref().unwrap_or(&[]),
            token,
        )
        .await;
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
        let pending: Vec<_> = (1..=count).filter(|n| !cp.parts.contains_key(n)).collect();
        let completed = cp
            .parts
            .keys()
            .map(|n| part_size.min(size.saturating_sub((n - 1) * part_size)))
            .sum::<u64>();
        m.progress(id, completed, size)?;
        let prefetch = m.inner.core.settings()["uploadPartConcurrency"]
            .as_u64()
            .unwrap_or(2)
            .clamp(1, 4)
            .min((32 * 1024 * 1024 / part_size).max(1)) as usize;
        let (send, mut receive) = tokio::sync::mpsc::channel::<Result<(u64, Vec<u8>)>>(prefetch);
        let worker_token = token.child_token();
        let producer_token = worker_token.clone();
        let path = local.clone();
        let producer = tokio::spawn(async move {
            let result=async {
                let mut file=fs::File::open(path).await.map_err(io_err)?;
                for number in pending {check(&producer_token)?;let start=(number-1)*part_size;let len=part_size.min(size-start);let mut bytes=vec![0;len as usize];file.seek(std::io::SeekFrom::Start(start)).await.map_err(io_err)?;file.read_exact(&mut bytes).await.map_err(io_err)?;tokio::select!{_ = producer_token.cancelled()=>return Err(err("ABORTED","上传预读已停止")),r=send.send(Ok((number,bytes)))=>{if r.is_err(){return Ok(());}}}}
                Ok(())
            }.await;
            if let Err(error) = result {
                let _ = send.send(Err(error)).await;
            }
        });
        let result = async {
            let mut uploaded = completed;
            loop {
                let next = cancellable(token, async { Ok(receive.recv().await) }).await?;
                let Some(part) = next else {
                    break;
                };
                let (number, bytes) = part?;
                // Actual PUTs are deliberately sequential. PDS has been observed returning PartNotSequential for concurrent PUTs.
                let etag = put_part(
                    m,
                    &pre,
                    &base,
                    &mime,
                    bucket,
                    key,
                    upload_id,
                    number,
                    bytes.clone().into(),
                    token,
                )
                .await?;
                uploaded += bytes.len() as u64;
                cp.parts.insert(number, etag);
                store(m, id, &cp)?;
                m.progress(id, uploaded, size)?;
            }
            Ok(())
        }
        .await;
        worker_token.cancel();
        drop(receive);
        producer
            .await
            .map_err(|_| err("LOCAL_IO", "上传预读任务异常"))?;
        result?;
        if cp.parts.len() as u64 != count {
            return Err(err("CONTENT_MISMATCH", "上传分片未完整提交"));
        }
        let current = fs::metadata(&local).await.map_err(io_err)?;
        let current_modified = current
            .modified()
            .map_err(io_err)?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        if current.len() != size || current_modified != modified {
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
        )
        .await?;
        let mut url = base.clone();
        url.query_pairs_mut().append_pair("uploadId", upload_id);
        cp.phase = "complete_started".into();
        store(m, id, &cp)?;
        check(token)?;
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
            return Err(err(
                "SERVER",
                format!("对象存储完成上传返回 HTTP {}", response.status().as_u16()),
            ));
        }
        let text = response.text().await.map_err(net_err)?;
        if text.to_ascii_lowercase().contains("<error") {
            return Err(err("SERVER", "对象存储没有确认完成上传"));
        }
        cp.phase = "completed".into();
        store(m, id, &cp)?;
    }
    if cp.phase == "completed" {
        check(token)?;
        cp.phase = "finish_started".into();
        store(m, id, &cp)?;
        m.inner
            .core
            .post(
                "/1/clouddrive/file/upload/finish",
                json!({"obj_key":key,"task_id":task_id}),
            )
            .await?;
        cp.phase = "result".into();
        store(m, id, &cp)?;
    }
    m.progress(id, size, size)?;
    confirm(
        m,
        parent,
        name,
        size,
        cp.baseline.as_deref().unwrap_or(&[]),
        token,
    )
    .await
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
    token: &CancellationToken,
) -> Result<String> {
    for attempt in 0..3 {
        check(token)?;
        let date = httpdate::fmt_http_date(SystemTime::now());
        let auth = signature(
            m,
            pre,
            put_meta(mime, &date, bucket, key, number, upload_id),
            token,
        )
        .await?;
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("partNumber", &number.to_string())
            .append_pair("uploadId", upload_id);
        let result = cancellable(token, async {
            let response = m
                .inner
                .core
                .http_client()
                .put(url)
                .header("authorization", auth)
                .header("content-type", mime)
                .header("content-length", bytes.len())
                .header("x-oss-date", date)
                .header("x-oss-user-agent", OSS_USER_AGENT)
                .body(bytes.clone())
                .timeout(Duration::from_secs(180))
                .send()
                .await
                .map_err(net_err)?;
            if !response.status().is_success() {
                return Err(err(
                    if response.status().as_u16() == 403 {
                        "SIGNATURE_EXPIRED"
                    } else if response.status().is_server_error() {
                        "NETWORK"
                    } else {
                        "SERVER"
                    },
                    format!("上传分片返回 HTTP {}", response.status().as_u16()),
                ));
            }
            let etag = response
                .headers()
                .get("etag")
                .and_then(|s| s.to_str().ok())
                .map(|s| s.trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| err("SERVER", "上传分片未返回 ETag"))?;
            if !etag.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return Err(err("SERVER", "上传 ETag 格式无效"));
            }
            response.bytes().await.map_err(net_err)?;
            Ok(etag)
        })
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
async fn matching(
    m: &TransferManager,
    parent: &str,
    name: &str,
    token: &CancellationToken,
) -> Result<Vec<Value>> {
    use unicode_normalization::UnicodeNormalization;
    let normalized = name.nfc().collect::<String>();
    let mut matches = Vec::new();
    for page in 1..=1000 {
        let result = cancellable(
            token,
            m.inner
                .core
                .call("fs.list", json!({"pdirFid":parent,"page":page,"size":100})),
        )
        .await?;
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
    size: u64,
    baseline: &[Value],
    token: &CancellationToken,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        check(token)?;
        let files = matching(m, parent, name, token).await?;
        let changed: Vec<_> = files
            .iter()
            .filter(|item| {
                item["size"].as_u64() == Some(size)
                    && baseline
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
            return Ok(());
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
