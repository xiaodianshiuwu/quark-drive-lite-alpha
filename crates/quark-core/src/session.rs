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
            .all(|b| matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e))
}
fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request.as_bytes().get(cookie.len()) == Some(&b'/')))
}
fn cookie_identity(a: &Value, b: &Value) -> bool {
    a["name"] == b["name"] && a["domain"] == b["domain"] && a["path"] == b["path"]
}
pub(crate) fn cookie_header(doc: &Value) -> String {
    cookie_header_for_path(doc, "/1/clouddrive/")
}
pub(crate) fn cookie_header_for_path(doc: &Value, request_path: &str) -> String {
    cookie_header_matching(doc, "drive-pc.quark.cn", request_path)
}
pub(crate) fn cookie_header_for_url(doc: &Value, url: &reqwest::Url) -> String {
    let Some(host) = url.host_str() else {
        return String::new();
    };
    if url.scheme() != "https"
        || !trusted_domain(host)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return String::new();
    }
    cookie_header_matching(doc, host, url.path())
}
fn cookie_header_matching(doc: &Value, request_host: &str, request_path: &str) -> String {
    let now = now_ms();
    let mut cookies = doc["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["expires"].as_i64().map_or(true, |v| v <= 0 || v > now))
        .filter(|c| {
            let Some(name) = c["name"].as_str() else {
                return false;
            };
            let Some(value) = c["value"].as_str() else {
                return false;
            };
            let domain = c["domain"].as_str().unwrap_or("").trim_start_matches('.');
            // Missing-domain legacy/import records are API-host-only. Browser
            // hostOnly records never cross to sibling CDN or login hosts.
            let domain_matches = if domain.is_empty() {
                request_host == "drive-pc.quark.cn"
            } else if c["hostOnly"] == true {
                request_host == domain
            } else {
                request_host == domain
                    || request_host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            };
            cookie_valid(name, value)
                && domain_matches
                && path_matches(request_path, c["path"].as_str().unwrap_or("/"))
        })
        .collect::<Vec<_>>();
    // RFC 6265 ordering: a specific-path value precedes a same-name root value.
    cookies.sort_by_key(|c| std::cmp::Reverse(c["path"].as_str().unwrap_or("/").len()));
    cookies
        .iter()
        .map(|c| {
            format!(
                "{}={}",
                c["name"].as_str().unwrap(),
                c["value"].as_str().unwrap()
            )
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
    if list.len() > 1024 {
        return Err(Error::new("INVALID_ARG", "Cookie 数量过多"));
    }
    let mut out = Vec::<Value>::new();
    let mut total_bytes = 0usize;
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
        total_bytes = total_bytes.saturating_add(name.len() + value.len() + 2);
        if total_bytes > 64 * 1024 {
            return Err(Error::new("INVALID_ARG", "Cookie 资料过大"));
        }
        let record = json!({"name":name,"value":value,"domain":domain,"path":c["path"].as_str().filter(|p|p.starts_with('/')).unwrap_or("/"),"expires":expires,"hostOnly":c["hostOnly"]==true,"httpOnly":c["httpOnly"]==true,"secure":c["secure"]==true,"sameSite":c["sameSite"]});
        if let Some(index) = out.iter().position(|p| cookie_identity(p, &record)) {
            out[index] = record;
        } else {
            out.push(record);
        }
    }
    Ok(Value::Array(out))
}
pub(crate) fn merge_set_cookie(doc: &mut Value, headers: &[String], response_path: &str) {
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
        let path = parsed
            .path()
            .filter(|p| p.starts_with('/'))
            .unwrap_or_else(|| {
                response_path
                    .rsplit_once('/')
                    .map(|(prefix, _)| if prefix.is_empty() { "/" } else { prefix })
                    .unwrap_or("/")
            });
        let record = json!({"name":parsed.name(),"value":parsed.value(),"domain":domain,"path":path,"expires":expires,"hostOnly":parsed.domain().is_none(),"httpOnly":parsed.http_only().unwrap_or(false),"secure":parsed.secure().unwrap_or(false),"sameSite":parsed.same_site().map(|s|s.to_string())});
        let old = cookies.iter().position(|c| cookie_identity(c, &record));
        if expires.is_some_and(|v| v <= now) {
            if let Some(index) = old {
                cookies.remove(index);
            }
            continue;
        }
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
        json!({"downloadDir":dirs::download_dir().unwrap_or_else(std::env::temp_dir).to_string_lossy(),"uploadConcurrency":2,"downloadConcurrency":2,"downloadParallel":16,"downloadStrategy":"auto","uploadPartConcurrency":3,"theme":"light"}),
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
    if let Some(strategy) = p
        .get("downloadStrategy")
        .and_then(Value::as_str)
        .filter(|s| ["auto", "sequential", "parallel"].contains(s))
    {
        base["downloadStrategy"] = json!(strategy);
    }
    if let Some(theme) = p.get("theme") {
        base["theme"] = json!(if theme == "dark" { "dark" } else { "light" });
    }
    base
}
pub(crate) fn merge_cached_user_info(info: &mut Value, cached: &Value) {
    // Reuse only normalized fields of this same generation, never raw API data.
    for key in ["nickname", "avatar", "memberLabel", "userId"] {
        if info[key].as_str().is_none_or(str::is_empty) {
            if let Some(value) = cached[key].as_str().filter(|s| !s.is_empty()) {
                info[key] = json!(value);
            }
        }
    }
}
pub(crate) fn merge_user_info(out: &mut Value, env: &Value) {
    let mut candidates = vec![&env["data"], &env["metadata"], env];
    for key in ["user_info", "userInfo", "member_info", "memberInfo"] {
        candidates.push(&env[key]);
        candidates.push(&env["data"][key]);
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
            ("userId", &["user_id", "userId", "uid"][..]),
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
        let mut doc =
            json!({"cookies":[{"name":"a","value":"old","domain":"quark.cn","path":"/"}]});
        merge_set_cookie(
            &mut doc,
            &["a=new; Domain=.quark.cn; Path=/; Max-Age=60; Expires=Wed, 01 Jan 2020 00:00:00 GMT".into()],
            "/1/clouddrive/config",
        );
        assert_eq!(cookie_header(&doc), "a=new");
        merge_set_cookie(
            &mut doc,
            &["a=gone; Domain=.quark.cn; Path=/; Max-Age=0".into()],
            "/1/clouddrive/config",
        );
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
    #[test]
    fn scoped_cookie_rotation_does_not_destroy_sibling_or_specific_path_cookie() {
        let cookies = normalize_external(json!([
            {"name":"same","value":"root","domain":"quark.cn","path":"/"},
            {"name":"same","value":"sibling","domain":"pan.quark.cn","path":"/"},
            {"name":"same","value":"specific","domain":"drive-pc.quark.cn","path":"/1/clouddrive/file"},
            {"name":"host","value":"only","domain":"quark.cn","path":"/","hostOnly":true}
        ])).unwrap();
        assert_eq!(cookies.as_array().unwrap().len(), 4);
        let mut doc = json!({"cookies":cookies});
        assert_eq!(
            cookie_header_for_path(&doc, "/1/clouddrive/file/sort"),
            "same=specific; same=root"
        );
        assert_eq!(
            cookie_header_for_path(&doc, "/1/clouddrive/filename"),
            "same=root"
        );
        merge_set_cookie(
            &mut doc,
            &["same=rotated; Domain=.quark.cn; Path=/".into()],
            "/1/clouddrive/config",
        );
        assert_eq!(
            cookie_header_for_path(&doc, "/1/clouddrive/file/sort"),
            "same=specific; same=rotated"
        );
        merge_set_cookie(
            &mut doc,
            &["same=deleted; Domain=.quark.cn; Path=/; Max-Age=0".into()],
            "/1/clouddrive/config",
        );
        assert_eq!(
            cookie_header_for_path(&doc, "/1/clouddrive/file/sort"),
            "same=specific"
        );
        assert!(doc["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["value"] == "sibling"));
    }

    #[test]
    fn settings_strategies_and_stable_identity_have_no_credential_fallback() {
        let defaults = settings(&json!({}));
        assert_eq!(defaults["downloadParallel"], 16);
        assert_eq!(defaults["uploadPartConcurrency"], 3);
        assert_eq!(defaults["downloadStrategy"], "auto");
        let patched = merge_settings(defaults, json!({"downloadStrategy":"parallel"}));
        assert_eq!(patched["downloadStrategy"], "parallel");
        assert_eq!(
            merge_settings(patched, json!({"downloadStrategy":"arbitrary"}))["downloadStrategy"],
            "parallel"
        );
        let mut account = json!({"userId":""});
        merge_user_info(
            &mut account,
            &json!({"data":{"id":"plan-id", "member_id":"membership-id"},"metadata":{"id":"request-id"}}),
        );
        assert_eq!(account["userId"], "");
        merge_user_info(
            &mut account,
            &json!({"data":{"user_info":{"uid":"stable-user-id"}}}),
        );
        assert_eq!(account["userId"], "stable-user-id");
    }
    #[test]
    fn download_url_cookie_scope_never_leaks_api_host_only_or_path_credentials() {
        let doc = json!({"cookies":[
            {"name":"api", "value":"only", "domain":"drive-pc.quark.cn", "path":"/", "hostOnly":true},
            {"name":"parent", "value":"shared", "domain":"quark.cn", "path":"/"},
            {"name":"file", "value":"scoped", "domain":"quark.cn", "path":"/1/clouddrive/file"},
            {"name":"pan", "value":"login", "domain":"pan.quark.cn", "path":"/", "hostOnly":true}
        ]});
        let parse = |s| reqwest::Url::parse(s).unwrap();
        assert_eq!(
            cookie_header_for_url(&doc, &parse("https://dl-pc.drive.quark.cn/object")),
            "parent=shared"
        );
        assert_eq!(
            cookie_header_for_url(
                &doc,
                &parse("https://drive-pc.quark.cn/1/clouddrive/file/sort")
            ),
            "file=scoped; api=only; parent=shared"
        );
        for url in [
            "http://drive-pc.quark.cn/1/clouddrive/file",
            "https://quark.cn.attacker.example/",
            "https://attacker.example/",
            "https://name@drive-pc.quark.cn/",
        ] {
            assert_eq!(cookie_header_for_url(&doc, &parse(url)), "");
        }
    }
    #[test]
    fn partial_metadata_refresh_preserves_stable_identity_but_updates_present_values() {
        let mut info = json!({"nickname":"new-name", "avatar":"", "memberLabel":"", "userId":""});
        merge_cached_user_info(
            &mut info,
            &json!({"nickname":"old-name", "userId":"stable-id", "cookie":"never-copy"}),
        );
        assert_eq!(info["nickname"], "new-name");
        assert_eq!(info["userId"], "stable-id");
        assert!(info.get("cookie").is_none());
    }
}
