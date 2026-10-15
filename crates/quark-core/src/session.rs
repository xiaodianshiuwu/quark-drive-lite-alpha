use crate::{Error, Result};
use cookie::Cookie;
use serde_json::{json, Value};

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn millis(value: &Value) -> Option<i64> {
    let n = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))?;
    if !n.is_finite() || n <= 0.0 {
        None
    } else {
        Some(if n < 1e11 {
            (n * 1000.0).round() as i64
        } else {
            n.round() as i64
        })
    }
}
fn trusted_domain(domain: &str) -> bool {
    domain.is_empty() || domain == "quark.cn" || domain.ends_with(".quark.cn")
}
fn cookie_valid(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && value
            .bytes()
            .all(|b| b >= 0x21 && b != b';' && b != 0x7f && b != b'\r' && b != b'\n')
}
pub(crate) fn cookie_header(doc: &Value) -> String {
    let now = now_ms();
    doc["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["expires"].as_i64().map_or(true, |v| v <= 0 || v > now))
        .filter_map(|c| {
            let name = c["name"].as_str()?;
            let value = c["value"].as_str()?;
            let domain = c["domain"].as_str().unwrap_or("").trim_start_matches('.');
            // Credentials are used only by the exact API origin. Browser imports may
            // include unrelated Quark login hosts; these are preserved, not sent.
            let domain_matches =
                domain.is_empty() || domain == "quark.cn" || domain == "drive-pc.quark.cn";
            let path = c["path"].as_str().unwrap_or("/");
            if !cookie_valid(name, value)
                || !domain_matches
                || !(path == "/" || "/1/clouddrive/".starts_with(path))
            {
                return None;
            }
            Some(format!("{name}={value}"))
        })
        .collect::<Vec<_>>()
        .join("; ")
}
pub(crate) fn account_state(doc: &Value) -> Value {
    json!({"loggedIn":!cookie_header(doc).is_empty() && doc["expired"] != true,"expired":doc["expired"] == true && doc["cookies"].as_array().is_some_and(|a| !a.is_empty()),"nickname":doc["account"]["nickname"].as_str().unwrap_or(""),"avatar":doc["account"]["avatar"].as_str().unwrap_or(""),"memberLabel":doc["account"]["memberLabel"].as_str().unwrap_or(""),"userId":doc["account"]["userId"].as_str().unwrap_or("")})
}
pub(crate) fn normalize_external(value: Value) -> Result<Value> {
    let list = value
        .as_array()
        .ok_or_else(|| Error::new("INVALID_ARG", "Cookie 必须为数组"))?;
    let mut out = Vec::<Value>::new();
    for c in list {
        let name = c["name"].as_str().unwrap_or("").trim();
        let value = c["value"].as_str().unwrap_or("");
        let domain = c["domain"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if !cookie_valid(name, value) || !trusted_domain(&domain) {
            continue;
        }
        let expires = millis(
            c.get("expires")
                .or_else(|| c.get("expirationDate"))
                .unwrap_or(&Value::Null),
        );
        if expires.is_some_and(|v| v <= now_ms()) {
            continue;
        }
        let record = json!({"name":name,"value":value,"domain":domain,"path":c["path"].as_str().filter(|p|p.starts_with('/')).unwrap_or("/"),"expires":expires,"httpOnly":c["httpOnly"]==true,"secure":c["secure"]==true,"sameSite":c["sameSite"]});
        if let Some(index) = out.iter().position(|p| p["name"] == name) {
            out[index] = record;
        } else {
            out.push(record);
        }
    }
    Ok(Value::Array(out))
}
pub(crate) fn merge_set_cookie(doc: &mut Value, headers: &[String]) {
    let mut cookies = doc["cookies"].as_array().cloned().unwrap_or_default();
    let now = now_ms();
    for raw in headers {
        let Ok(parsed) = Cookie::parse(raw.as_str()) else {
            continue;
        };
        if !cookie_valid(parsed.name(), parsed.value()) {
            continue;
        }
        let domain = parsed
            .domain()
            .unwrap_or("drive-pc.quark.cn")
            .trim_start_matches('.')
            .to_ascii_lowercase();
        // A response from drive-pc.quark.cn cannot set arbitrary sibling hosts.
        if domain != "quark.cn" && domain != "drive-pc.quark.cn" {
            continue;
        }
        let expires = if let Some(age) = parsed.max_age() {
            Some(now.saturating_add(age.whole_seconds().saturating_mul(1000)))
        } else {
            parsed
                .expires_datetime()
                .map(|d| (d.unix_timestamp_nanos() / 1_000_000) as i64)
        };
        let old = cookies.iter().position(|c| c["name"] == parsed.name());
        if expires.is_some_and(|v| v <= now) {
            if let Some(index) = old {
                cookies.remove(index);
            }
            continue;
        }
        let record = json!({"name":parsed.name(),"value":parsed.value(),"domain":domain,"path":parsed.path().unwrap_or("/"),"expires":expires,"httpOnly":parsed.http_only().unwrap_or(false),"secure":parsed.secure().unwrap_or(false),"sameSite":parsed.same_site().map(|s|s.to_string())});
        if let Some(index) = old {
            cookies[index] = record;
        } else {
            cookies.push(record);
        }
    }
    doc["cookies"] = Value::Array(cookies);
}
pub(crate) fn settings(doc: &Value) -> Value {
    merge_settings(
        json!({"downloadDir":dirs::download_dir().unwrap_or_else(std::env::temp_dir).to_string_lossy(),"uploadConcurrency":2,"downloadConcurrency":2,"downloadParallel":4,"uploadPartConcurrency":2,"theme":"light"}),
        doc["settings"].clone(),
    )
}
pub(crate) fn merge_settings(mut base: Value, patch: Value) -> Value {
    let Some(p) = patch.as_object() else {
        return base;
    };
    if let Some(dir) = p
        .get("downloadDir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && !s.contains('\0'))
    {
        base["downloadDir"] = json!(dir);
    }
    for (key, max) in [
        ("uploadConcurrency", 8),
        ("downloadConcurrency", 5),
        ("downloadParallel", 16),
        ("uploadPartConcurrency", 8),
    ] {
        if let Some(value) = p
            .get(key)
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .filter(|v| v.is_finite())
        {
            base[key] = json!((value as i64).clamp(1, max));
        }
    }
    if let Some(theme) = p.get("theme") {
        base["theme"] = json!(if theme == "dark" { "dark" } else { "light" });
    }
    base
}
pub(crate) fn merge_user_info(out: &mut Value, env: &Value) {
    let mut candidates = vec![&env["data"], &env["metadata"], env];
    for key in ["user_info", "userInfo", "member_info", "memberInfo"] {
        candidates.push(&env[key]);
    }
    for key in ["member", "user", "profile", "account"] {
        candidates.push(&env["data"][key]);
    }
    for value in candidates {
        for (target, keys) in [
            (
                "nickname",
                &[
                    "nickname",
                    "nick_name",
                    "nickName",
                    "user_name",
                    "userName",
                    "name",
                    "display_name",
                    "displayName",
                ][..],
            ),
            (
                "avatar",
                &[
                    "avatar",
                    "avatar_url",
                    "avatarUrl",
                    "head_image",
                    "headImage",
                    "head_img",
                    "headImg",
                    "portrait",
                    "icon",
                ][..],
            ),
            (
                "memberLabel",
                &[
                    "member_label",
                    "memberLabel",
                    "member_type_name",
                    "memberTypeName",
                    "vip_label",
                    "vipLabel",
                    "vip_name",
                    "vipName",
                    "member_name",
                    "memberName",
                    "level_name",
                    "levelName",
                    "title",
                ][..],
            ),
            (
                "userId",
                &["user_id", "userId", "uid", "member_id", "memberId", "id"][..],
            ),
        ] {
            if out[target].as_str().is_some_and(|s| !s.is_empty()) {
                continue;
            }
            if let Some(s) = keys
                .iter()
                .filter_map(|key| {
                    value[*key]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.trim().to_owned())
                        .or_else(|| value[*key].as_i64().map(|n| n.to_string()))
                })
                .next()
            {
                out[target] = json!(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_seconds_and_api_scope() {
        let out = normalize_external(json!([
            {"name":"a","value":"one","domain":".quark.cn","expires":now_ms()/1000+60},
            {"name":"b","value":"two","domain":"pan.quark.cn","expirationDate":now_ms()/1000+60},
            {"name":"c","value":"evil","domain":"attacker.example"},
            {"name":"d","value":"bad\r\nInjected: value","domain":".quark.cn"}
        ]))
        .unwrap();
        assert_eq!(out.as_array().unwrap().len(), 2);
        assert!(out[0]["expires"].as_i64().unwrap() > now_ms());
        assert_eq!(cookie_header(&json!({"cookies":out})), "a=one");
    }
    #[test]
    fn rotation_and_max_age_precedence() {
        let mut doc = json!({"cookies":[{"name":"a","value":"old","domain":"quark.cn"}]});
        merge_set_cookie(
            &mut doc,
            &["a=new; Domain=.quark.cn; Max-Age=60; Expires=Wed, 01 Jan 2020 00:00:00 GMT".into()],
        );
        assert_eq!(cookie_header(&doc), "a=new");
        merge_set_cookie(&mut doc, &["a=gone; Max-Age=0".into()]);
        assert_eq!(cookie_header(&doc), "");
    }
    #[test]
    fn settings_bounds_and_unknown_field_rejection() {
        let s = merge_settings(
            settings(&json!({})),
            json!({"uploadConcurrency":999,"downloadParallel":0,"downloadDir":false,"theme":"dark","cookie":"never"}),
        );
        assert_eq!(s["uploadConcurrency"], 8);
        assert_eq!(s["downloadParallel"], 1);
        assert_eq!(s["theme"], "dark");
        assert!(s.get("cookie").is_none());
        assert!(s["downloadDir"].is_string());
    }
}
