use crate::{Core, Error, Result};
use serde_json::{json, Value};

pub(crate) fn number(value: &Value, fallback: i64) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(fallback)
}
fn text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        Value::String(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1"
        ),
        _ => false,
    }
}
fn required(value: &Value, label: &str) -> Result<String> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| Error::new("INVALID_ARG", format!("{label}不能为空")))
}
fn ids(value: &Value) -> Result<Vec<String>> {
    let ids = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_owned())
        })
        .collect::<Vec<_>>();
    if ids.is_empty() {
        Err(Error::new("INVALID_ARG", "请先选择要操作的文件"))
    } else {
        Ok(ids)
    }
}
fn list(env: &Value) -> Option<&Vec<Value>> {
    env["data"]["list"]
        .as_array()
        .or_else(|| env["data"].as_array())
        .or_else(|| env["metadata"]["list"].as_array())
        .or_else(|| env["list"].as_array())
}
fn record(env: &Value) -> Option<&Value> {
    let data = &env["data"];
    if let Some(a) = data.as_array() {
        a.first()
    } else if let Some(a) = data["list"].as_array() {
        a.first()
    } else if data.as_object().is_some_and(|o| !o.is_empty()) {
        Some(data)
    } else {
        env["metadata"]["list"].as_array().and_then(|a| a.first())
    }
}
fn task_id(value: &Value) -> String {
    let direct = text(value.get("task_id").unwrap_or(&value["taskId"]));
    if !direct.is_empty() {
        direct
    } else {
        text(&value["task_ids"][0])
    }
}
fn accepted(env: &Value) -> Value {
    let id = record(env)
        .map(task_id)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| task_id(env));
    if id.is_empty() {
        json!({"done":true})
    } else {
        json!({"done":false,"taskId":id})
    }
}
fn task_entry(value: &Value) -> Result<Value> {
    let status = value["status"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let finished = truth(&value["finish"]) || truth(&value["done"]) || status == "finish";
    // Finishing is not synonymous with succeeding. Explicit failure wins over
    // completion flags, while undocumented numeric states remain unresolved.
    if truth(&value["failed"])
        || ["failed", "failure", "error", "cancelled", "canceled"].contains(&status.as_str())
        || (finished && value["success"] == false)
    {
        return Err(Error::new("TASK_FAILED", "云端文件操作失败，请刷新后重试"));
    }
    Ok(
        json!({"taskId":task_id(value),"status":value.get("status").unwrap_or(&json!(-1)),"done":finished||truth(&value["success"]),"raw":value}),
    )
}
pub(crate) fn file_item(raw: &Value) -> Value {
    let original = raw["file_name"]
        .as_str()
        .or_else(|| raw["name"].as_str())
        .unwrap_or("");
    let decoded = html_escape::decode_html_entities(original);
    let name = if decoded.is_empty() {
        "未命名"
    } else {
        decoded.as_ref()
    };
    let dir = if !raw["dir"].is_null() {
        truth(&raw["dir"])
    } else {
        (!raw["file_type"].is_null() && number(&raw["file_type"], -1) == 0) || raw["file"] == false
    };
    let ext = if dir {
        "".into()
    } else {
        name.rsplit('/')
            .next()
            .and_then(|s| {
                s.rfind('.')
                    .filter(|i| *i > 0 && *i + 1 < s.len())
                    .map(|i| s[i + 1..].to_lowercase())
            })
            .unwrap_or_default()
    };
    let mut item = json!({"fid":text(&raw["fid"]),"name":name,"dir":dir,"size":if dir {0}else{number(&raw["size"],0).max(0)},"updatedAt":number(raw.get("updated_at").unwrap_or(&raw["updatedAt"]),0),"createdAt":number(raw.get("created_at").unwrap_or(&raw["createdAt"]),0),"category":number(&raw["category"],0),"ext":ext,"pdirFid":text(raw.get("pdir_fid").unwrap_or(&raw["pdirFid"]))});
    // Preserve confirmed digest fields for the transfer core; the renderer may
    // ignore them. Reject malformed values instead of weakening verification.
    for (key, len) in [("md5", 32), ("sha1", 40)] {
        if let Some(hash) = raw[key]
            .as_str()
            .filter(|s| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            item[key] = json!(hash.to_ascii_lowercase());
        }
    }
    item
}
fn page_result(env: &Value, page: i64, size: i64) -> Value {
    let entries = list(env)
        .into_iter()
        .flatten()
        .map(file_item)
        .collect::<Vec<_>>();
    let total = [
        &env["metadata"]["_total"],
        &env["metadata"]["total"],
        &env["total"],
        &env["data"]["_total"],
    ]
    .iter()
    .filter(|v| !v.is_null())
    .map(|v| number(v, -1))
    .find(|n| *n >= 0)
    .unwrap_or(-1);
    let has_more = if total >= 0 {
        page.saturating_mul(size) < total
    } else {
        entries.len() == size as usize
    };
    json!({"list":entries,"total":total,"page":page,"size":size,"hasMore":has_more})
}
impl Core {
    async fn file_page(&self, generation: u64, params: &Value, search: bool) -> Result<Value> {
        let page = number(&params["page"], 1).max(1);
        let size = number(&params["size"], 50).clamp(1, 1000);
        let mut query = vec![
            ("_page", page.to_string()),
            ("_size", size.to_string()),
            ("_fetch_total", "1".into()),
        ];
        let path = if search {
            query.push(("q", required(&params["keyword"], "搜索关键字")?));
            "/1/clouddrive/file/search"
        } else {
            query.extend([
                (
                    "pdir_fid",
                    params["pdirFid"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("0")
                        .into(),
                ),
                (
                    "_sort",
                    params["sort"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("file_type:asc,updated_at:desc")
                        .into(),
                ),
                ("fetch_all_file", "1".into()),
                ("fetch_risk_file_name", "1".into()),
            ]);
            "/1/clouddrive/file/sort"
        };
        if !params["cat"].is_null() {
            let cat = text(&params["cat"]);
            if !cat.is_empty() {
                query.push(("cat", cat));
            }
        }
        let env = self.get_in_session(generation, path, &query).await?;
        if list(&env).is_none() {
            return Err(Error::new(
                "SERVER",
                if search {
                    "搜索响应缺少文件列表，暂时无法确认搜索结果"
                } else {
                    "目录响应缺少文件列表，请刷新重试"
                },
            ));
        }
        Ok(page_result(&env, page, size))
    }
    async fn task_status(&self, generation: u64, params: &Value) -> Result<Value> {
        let env = self
            .post_in_session(
                generation,
                "/1/clouddrive/task/status",
                json!({"task_ids":ids(&params["taskIds"])? ,"op_type":number(&params["opType"],1)}),
            )
            .await?;
        if let Some(a) = list(&env) {
            Ok(Value::Array(
                a.iter().map(task_entry).collect::<Result<Vec<_>>>()?,
            ))
        } else if let Some(v) =
            record(&env).filter(|r| r.get("task_id").is_some() || r.get("status").is_some())
        {
            Ok(json!([task_entry(v)?]))
        } else {
            Ok(json!([]))
        }
    }
    pub(crate) async fn files_call(&self, method: &str, params: Value) -> Result<Value> {
        let generation = self.session_generation();
        if self.cookie_header().is_empty() || self.call_status()["expired"] == true {
            return Err(Error::new("NEED_LOGIN", "请先登录夸克网盘"));
        }
        match method {
            "fs.list"=>self.file_page(generation,&params,false).await,
            "fs.search"=>self.file_page(generation,&params,true).await,
            "fs.info"=>{
                let fid=required(if params.is_string(){&params}else{&params["fid"]},"文件 fid")?;
                let env=self.get_in_session(generation,"/1/clouddrive/file/info",&[("fid",fid),("_fetch_full_path","1".into()),("need_profile_tags","1".into()),("kkkk","1".into())]).await?;
                record(&env).map(file_item).ok_or_else(||Error::new("NOT_FOUND","未找到该文件，可能已被删除或移动"))
            }
            "fs.crumbs"=>{
                let fid=if params.is_string(){text(&params)}else{text(&params["fid"])};
                let root=json!([{"fid":"0","name":"全部文件"}]);if fid.is_empty()||fid=="0"{return Ok(root);}
                match self.get_in_session(generation,"/1/clouddrive/file/info/path_list",&[("fid",fid)]).await {
                    Ok(env)=>{let out=list(&env).into_iter().flatten().filter_map(|v|{let fid=text(v.get("fid").unwrap_or(&v["id"]));if fid.is_empty(){None}else{Some(json!({"fid":fid,"name":text(v.get("file_name").unwrap_or(&v["name"]))}))}}).collect::<Vec<_>>();Ok(if out.is_empty(){root}else{json!(out)})}
                    Err(e) if e.code=="NEED_LOGIN"||e.code=="ABORTED"=>Err(e),Err(_)=>Ok(root)
                }
            }
            "fs.mkdir"=>Ok(accepted(&self.post_in_session(generation,"/1/clouddrive/file",json!({"pdir_fid":params["pdirFid"].as_str().filter(|s|!s.is_empty()).unwrap_or("0"),"file_name":required(&params["name"],"文件夹名称")?,"dir_path":"","dir_init_lock":false})).await?)),
            "fs.rename"=>{required(&params["name"],"新名称")?;Ok(accepted(&self.post_in_session(generation,"/1/clouddrive/file/rename",json!({"fid":required(&params["fid"],"文件 fid")?,"file_name":params["name"]})).await?))},
            "fs.move"=>Ok(accepted(&self.post_in_session(generation,"/1/clouddrive/file/move",json!({"action_type":1,"to_pdir_fid":required(&params["toFid"],"目标目录 fid")?,"filelist":ids(&params["fids"])? ,"exclude_fids":[]})).await?)),
            "fs.copy"=>Ok(accepted(&self.post_in_session(generation,"/1/clouddrive/file/copy",json!({"action_type":2,"to_pdir_fid":required(&params["toFid"],"目标目录 fid")?,"filelist":ids(&params["fids"])? ,"copy_source":1})).await?)),
            "fs.remove"=>Ok(accepted(&self.post_in_session(generation,"/1/clouddrive/file/delete",json!({"action_type":2,"filelist":ids(&params["fids"])? ,"exclude_fids":[]})).await?)),
            "fs.taskStatus"=>self.task_status(generation,&params).await,
            "fs.queryTask"=>{
                let id=required(if params.is_string(){&params}else{&params["taskId"]},"任务 id")?;
                let env=self.get_in_session(generation,"/1/clouddrive/task",&[("task_id",id)]).await?;
                task_entry(record(&env).ok_or_else(||Error::new("NOT_FOUND","未找到该任务，可能已结束或被清理"))?)
            }
            "fs.waitTask"=>{
                let mut pending=ids(&params["taskIds"])?;let start=std::time::Instant::now();
                let timeout=number(&params["timeout"],180000).max(0) as u64;let interval=number(&params["interval"],1000).clamp(1,10000) as u64;
                loop {
                    let entries=self.task_status(generation,&json!({"taskIds":pending,"opType":number(&params["opType"],1)})).await?;
                    pending.retain(|id|!entries.as_array().unwrap().iter().any(|v|v["taskId"]==id.as_str()&&v["done"]==true));
                    if pending.is_empty(){return Ok(json!({"done":true,"entries":entries,"pending":[],"elapsed":start.elapsed().as_millis() as u64}));}
                    if start.elapsed().as_millis()>=timeout as u128{return Err(Error::new("TASK_FAILED","操作超时，任务可能仍在后台执行，请稍后刷新查看"));}
                    tokio::time::sleep(std::time::Duration::from_millis(interval.min(timeout.saturating_sub(start.elapsed().as_millis() as u64)))).await;
                }
            }
            "fs.listAll"=>{
                let size=number(&params["pageSize"],100).clamp(1,1000);let max_pages=number(&params["maxPages"],500).clamp(1,10000);
                let mut out=Vec::new();let mut page_params=if params.is_object(){params.clone()}else{json!({})};let mut total=-1;let mut truncated=false;
                for page in 1..=max_pages {page_params["page"]=json!(page);page_params["size"]=json!(size);let p=self.file_page(generation,&page_params,false).await?;let list=p["list"].as_array().unwrap();total=number(&p["total"],-1);out.extend(list.iter().cloned());if list.is_empty()||p["hasMore"]!=true{break;}if page==max_pages{truncated=true;}}
                Ok(json!({"list":out,"total":if total>=0{total}else{out.len() as i64},"page":1,"size":size,"hasMore":truncated,"truncated":truncated}))
            }
            _=>Err(Error::new("INVALID_ARG","未知文件操作"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_directory_primary_field_wins() {
        let item = file_item(
            &json!({"fid":"1","dir":false,"file_type":0,"file_name":"A&amp;B.TXT","size":"99","md5":"00000000000000000000000000000000","sha1":"malformed"}),
        );
        assert_eq!(item["dir"], false);
        assert_eq!(item["name"], "A&B.TXT");
        assert_eq!(item["ext"], "txt");
        assert_eq!(item["size"], 99);
        assert_eq!(item["md5"], "00000000000000000000000000000000");
        assert!(item.get("sha1").is_none());
    }
    #[test]
    fn paging_uses_metadata_and_preserves_unknown() {
        let p = page_result(
            &json!({"data":{"list":[{"fid":"1"}]},"metadata":{"_total":"100"}}),
            1,
            50,
        );
        assert_eq!(p["total"], 100);
        assert_eq!(p["hasMore"], true);
        let p = page_result(&json!({"data":[]}), 1, 50);
        assert_eq!(p["total"], -1);
        assert_eq!(p["hasMore"], false);
    }
    #[test]
    fn unknown_numeric_task_status_never_means_success() {
        assert_eq!(
            task_entry(&json!({"task_id":"t","status":2})).unwrap()["done"],
            false
        );
        assert_eq!(
            task_entry(&json!({"task_id":"t","status":"finish"})).unwrap()["done"],
            true
        );
        assert_eq!(
            accepted(&json!({"data":{"task_id":"t"}})),
            json!({"taskId":"t","done":false})
        );
    }
    #[test]
    fn explicit_task_failure_wins_over_finish_and_success_flags() {
        for raw in [
            json!({"task_id":"t","finish":true,"success":false}),
            json!({"task_id":"t","done":true,"status":"failed"}),
            json!({"task_id":"t","success":true,"failed":true}),
        ] {
            assert_eq!(task_entry(&raw).unwrap_err().code, "TASK_FAILED");
        }
        assert_eq!(
            task_entry(&json!({"task_id":"t","status":2,"success":false})).unwrap()["done"],
            false
        );
        assert_eq!(
            task_entry(&json!({"task_id":"t","finish":true,"success":true})).unwrap()["done"],
            true
        );
    }
}
