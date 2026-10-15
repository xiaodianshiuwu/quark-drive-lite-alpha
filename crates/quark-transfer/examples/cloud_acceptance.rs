//! Explicit real-account acceptance. Only this run's UUID directory is modified.
use quark_core::{Core, Error, Result};
use quark_transfer::TransferManager;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn fail(message: &str) -> Error {
    Error::new("ACCEPTANCE", message)
}
fn io(_: std::io::Error) -> Error {
    fail("本地验收文件读写失败")
}
async fn list(core: &Core, parent: &str) -> Result<Vec<Value>> {
    Ok(
        core.call("fs.listAll", json!({"pdirFid":parent})).await?["list"]
            .as_array()
            .ok_or_else(|| fail("列表格式无效"))?
            .clone(),
    )
}
async fn find(core: &Core, parent: &str, name: &str) -> Result<Value> {
    for _ in 0..30 {
        let matches: Vec<_> = list(core, parent)
            .await?
            .into_iter()
            .filter(|f| f["name"] == name)
            .collect();
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if matches.len() > 1 {
            return Err(fail("测试目录出现重名，保留现场"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(fail("云端确认超时，测试现场已保留"))
}
fn hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(io)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
async fn wait(manager: &TransferManager, id: &str, pause: bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(900);
    let mut paused = false;
    loop {
        let tasks = manager.snapshot();
        let task = tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .ok_or_else(|| fail("任务丢失"))?
            .clone();
        match task["status"].as_str().unwrap_or("") {
            "done" => {
                println!("transfer complete; pause/resume exercised: {paused}");
                return Ok(task);
            }
            "error" | "canceled" => {
                return Err(Error::new(
                    task["errorCode"].as_str().unwrap_or("TRANSFER"),
                    task["error"].as_str().unwrap_or("任务失败"),
                ))
            }
            _ => {}
        }
        if pause
            && !paused
            && task["transferred"].as_u64().unwrap_or(0) > 0
            && task["transferred"] != task["size"]
        {
            manager.call("transfer.pause", json!({"id":id})).await?;
            manager.call("transfer.resume", json!({"id":id})).await?;
            paused = true;
        }
        if Instant::now() > deadline {
            manager.pause_all().await?;
            return Err(fail("验收超时，任务已暂停"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn run(
    core: &Core,
    manager: &TransferManager,
    work: &Path,
    ledger: &mut Value,
) -> Result<()> {
    let name = format!("qk-tauri-accept-{}", uuid::Uuid::new_v4());
    ledger["name"] = json!(name);
    fs::write(
        work.join("ledger.json"),
        serde_json::to_vec_pretty(ledger).unwrap(),
    )
    .map_err(io)?;
    core.call("fs.mkdir", json!({"pdirFid":"0","name":name}))
        .await?;
    let root = find(core, "0", &name).await?;
    let root_id = root["fid"].as_str().ok_or_else(|| fail("目录ID缺失"))?;
    ledger["fid"] = json!(root_id);
    fs::write(
        work.join("ledger.json"),
        serde_json::to_vec_pretty(ledger).unwrap(),
    )
    .map_err(io)?;
    let source = work.join("fixture-32MiB.bin");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source)
        .map_err(io)?;
    let mut state = uuid::Uuid::new_v4().as_u128() as u64;
    let mut buf = vec![0; 65536];
    for _ in 0..512 {
        for byte in &mut buf {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        file.write_all(&buf).map_err(io)?;
    }
    file.sync_all().map_err(io)?;
    drop(file);
    let expected = hash(&source)?;
    let ids = manager
        .call(
            "transfer.upload",
            json!({"pdirFid":root_id,"localPaths":[source]}),
        )
        .await?;
    wait(manager, ids[0].as_str().unwrap(), false).await?;
    let uploaded = find(core, root_id, "fixture-32MiB.bin").await?;
    let fid = uploaded["fid"].as_str().unwrap();
    if uploaded["size"] != 32 * 1024 * 1024 {
        return Err(fail("上传大小不符"));
    }
    core.call("fs.rename", json!({"fid":fid,"name":"renamed-32MiB.bin"}))
        .await?;
    find(core, root_id, "renamed-32MiB.bin").await?;
    let output = work.join("download");
    fs::create_dir_all(&output).map_err(io)?;
    let ids = manager
        .call("transfer.download", json!({"fids":[fid],"localDir":output}))
        .await?;
    let task = wait(manager, ids[0].as_str().unwrap(), true).await?;
    let actual = hash(Path::new(task["localPath"].as_str().unwrap()))?;
    if actual != expected {
        return Err(fail("真实上传下载SHA-256回读不符"));
    }
    ledger["verified"] = json!(true);
    ledger["size"] = json!(32 * 1024 * 1024);
    ledger["sha256"] = json!(actual);
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = execute().await {
        eprintln!("{}: {}", error.code, error.message);
        std::process::exit(1)
    }
}
async fn execute() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err(fail(
            "用法: cloud_acceptance <已扫码的profile绝对目录> <本轮新工作目录> [--inspect]",
        ));
    }
    let profile = PathBuf::from(&args[1]);
    let work = PathBuf::from(&args[2]);
    if !profile.is_absolute() || !work.is_absolute() {
        return Err(fail("需要绝对目录"));
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(profile.join("app.lock"))
        .map_err(io)?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| fail("请先关闭该资料目录的客户端"))?;
    let core = Core::open(&profile)?;
    if core.call("auth.verify", json!({"force":true})).await?["loggedIn"] != true {
        return Err(fail("请先在Tauri客户端扫码"));
    }
    core.call("auth.getUserInfo", Value::Null).await?;
    if args.iter().any(|s| s == "--inspect") {
        for endpoint in ["/1/clouddrive/member", "/1/clouddrive/config"] {
            let env = core.get(endpoint, &[]).await?;
            fn keys(value: &Value, prefix: &str, depth: usize) {
                if depth > 4 {
                    return;
                }
                if let Some(o) = value.as_object() {
                    for (k, v) in o {
                        let p = format!("{prefix}.{k}");
                        println!(
                            "{} {}",
                            p,
                            if v.is_object() {
                                "object"
                            } else if v.is_array() {
                                "array"
                            } else {
                                "scalar"
                            }
                        );
                        if v.is_object() {
                            keys(v, &p, depth + 1)
                        }
                    }
                }
            }
            println!("{endpoint}");
            keys(&env, "response", 0);
        }
        println!("identified={}", core.account_key().is_some());
        return Ok(());
    }
    if core.account_key().is_none() {
        return Err(fail("服务端未返回可信账号ID，禁止创建传输任务"));
    }
    if args
        .iter()
        .any(|s| s == "--cleanup" || s == "--check-cleanup")
    {
        let mut ledger: Value =
            serde_json::from_slice(&fs::read(work.join("ledger.json")).map_err(io)?)
                .map_err(|_| fail("本轮记录损坏"))?;
        let name = ledger["name"]
            .as_str()
            .filter(|s| s.starts_with("qk-tauri-accept-"))
            .ok_or_else(|| fail("测试目录名称不符"))?;
        let fid = ledger["fid"]
            .as_str()
            .ok_or_else(|| fail("没有测试目录ID"))?;
        if args.iter().any(|s| s == "--check-cleanup") {
            if list(&core, "0")
                .await?
                .iter()
                .any(|item| item["fid"] == fid)
            {
                return Err(fail("本轮测试目录仍存在"));
            }
            ledger["cleaned"] = json!(true);
            fs::write(
                work.join("ledger.json"),
                serde_json::to_vec_pretty(&ledger).unwrap(),
            )
            .map_err(io)?;
            println!("Confirmed ledger-owned test directory absent");
            return Ok(());
        }
        let info = core.call("fs.info", json!(fid)).await?;
        if info["name"] != name || info["dir"] != true {
            return Err(fail("测试目录归属变化，禁止删除"));
        }
        let manager = TransferManager::open(core.clone())?;
        let tasks = manager.snapshot();
        for t in tasks
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["pdirFid"] == fid)
        {
            manager
                .call("transfer.cancel", json!({"id":t["id"]}))
                .await?;
        }
        core.call("fs.remove", json!({"fids":[fid]})).await?;
        println!("Only ledger-owned test directory cleaned");
        return Ok(());
    }
    fs::create_dir(&work).map_err(io)?;
    let manager = TransferManager::open(core.clone())?;
    let mut ledger = json!({"verified":false,"cleaned":false});
    let result = run(&core, &manager, &work, &mut ledger).await;
    manager.pause_all().await?;
    if result.is_ok() {
        let fid = ledger["fid"]
            .as_str()
            .ok_or_else(|| fail("没有本轮目录记录"))?;
        let info = core.call("fs.info", json!(fid)).await?;
        if info["name"] != ledger["name"] || info["dir"] != true {
            return Err(fail("测试目录归属变化，禁止删除"));
        }
        core.call("fs.remove", json!({"fids":[fid]})).await?;
        ledger["cleaned"] = json!(true);
    }
    fs::write(
        work.join("ledger.json"),
        serde_json::to_vec_pretty(&ledger).unwrap(),
    )
    .map_err(io)?;
    result?;
    println!("32 MiB full SHA-256 roundtrip passed; unique cloud test directory cleaned");
    Ok(())
}
