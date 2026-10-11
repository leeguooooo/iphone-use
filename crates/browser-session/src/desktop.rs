//! Narrow, opt-in Chrome site access. Keys are paired out-of-band, never
//! sent as HTTP credentials. Requests/results are authenticated ciphertext.
use crate::{open_message, seal_message};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn default_scope_file() -> Result<PathBuf, &'static str> {
    Ok(std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("home_unavailable")?
        .join(".iphone-use/chrome-scopes.v1.json"))
}
fn allowed(host: &str, domains: &[String]) -> bool {
    domains
        .iter()
        .any(|d| host == d || host.ends_with(&format!(".{d}")))
}
fn read_scopes(file: &Path) -> Result<Value, &'static str> {
    if !file.exists() {
        return Ok(json!({"version":1,"scopes":[]}));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(file)
            .map_err(|_| "scope_unavailable")?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err("scope_file_permissions_too_open");
        }
    }
    serde_json::from_slice(&std::fs::read(file).map_err(|_| "scope_unavailable")?)
        .map_err(|_| "invalid_scope_file")
}
fn private_write(file: &Path, data: &[u8]) -> Result<(), &'static str> {
    use std::io::Write;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "scope_unavailable")?;
    }
    let temp = file.with_extension(format!("tmp-{}", hex::encode(rand::random::<[u8; 8]>())));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut writer = options.open(&temp).map_err(|_| "scope_unavailable")?;
    writer.write_all(data).map_err(|_| "scope_unavailable")?;
    writer.sync_all().map_err(|_| "scope_unavailable")?;
    std::fs::rename(temp, file).map_err(|_| "scope_unavailable")?;
    if let Some(parent) = file.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "scope_unavailable")?;
    }
    Ok(())
}
pub fn grant(
    file: &Path,
    sites: &Path,
    profile: &str,
    domains: &[String],
    writes: bool,
    endpoint: &str,
) -> Result<String, &'static str> {
    let _lock = scope_lock(file)?;
    let endpoint_url = url::Url::parse(endpoint).map_err(|_| "invalid_endpoint")?;
    if endpoint_url.scheme() != "https" && endpoint_url.scheme() != "http" {
        return Err("invalid_endpoint");
    }
    if endpoint_url.username() != ""
        || endpoint_url.password().is_some()
        || endpoint_url.query().is_some()
        || endpoint_url.fragment().is_some()
    {
        return Err("invalid_endpoint");
    }
    if domains.is_empty() || domains.len() > 30 || profile.is_empty() || profile.len() > 80 {
        return Err("invalid_scope");
    }
    if profile != "Default"
        && !(profile.starts_with("Profile ")
            && profile.len() > 8
            && profile[8..].bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("invalid_profile");
    }
    if domains.iter().any(|d| {
        d.len() > 253
            || !d.contains('.')
            || d.split('.').any(|p| {
                p.is_empty()
                    || p.starts_with('-')
                    || p.ends_with('-')
                    || !p
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
    }) {
        return Err("invalid_domain");
    }
    let pins = catalog(sites, domains)?
        .into_iter()
        .map(|v| {
            (
                v["spec"].as_str().unwrap().to_string(),
                v["fingerprint"].clone(),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    if pins.is_empty() {
        return Err("no_installed_sites_in_scope");
    }
    let mut key = [0; 32];
    rand::thread_rng().fill_bytes(&mut key);
    let id = hex::encode(Sha256::digest(key))[..24].to_string();
    let mut scopes = read_scopes(file)?;
    scopes["scopes"].as_array_mut().ok_or("invalid_scope_file")?.push(json!({"id":id,"key":STANDARD.encode(key),"profile":profile,"domains":domains,"writes":writes,"pins":pins,"expires":now()+7*86400}));
    private_write(
        file,
        &serde_json::to_vec(&scopes).map_err(|_| "invalid_scope_file")?,
    )?;
    let mut link = url::Url::parse("phoneuse-chrome://pair").unwrap();
    link.query_pairs_mut()
        .append_pair("u", endpoint)
        .append_pair("s", &id)
        .append_pair("k", &STANDARD.encode(key));
    Ok(link.into())
}
pub fn revoke(file: &Path, id: &str) -> Result<(), &'static str> {
    let _lock = scope_lock(file)?;
    let mut scopes = read_scopes(file)?;
    scopes["scopes"]
        .as_array_mut()
        .ok_or("invalid_scope_file")?
        .retain(|s| s["id"] != id);
    private_write(
        file,
        &serde_json::to_vec(&scopes).map_err(|_| "invalid_scope_file")?,
    )
}

fn adapter(root: &Path, spec: &str) -> Result<(Value, String, String), &'static str> {
    let parts: Vec<_> = spec.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || s.contains("..")
                || !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        })
    {
        return Err("invalid_spec");
    }
    let path = root.join(parts[0]).join(format!("{}.js", parts[1]));
    if path.is_symlink() {
        return Err("invalid_adapter");
    }
    let canonical_root = std::fs::canonicalize(root).map_err(|_| "invalid_adapter")?;
    let canonical_path = std::fs::canonicalize(&path).map_err(|_| "adapter_not_installed")?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err("invalid_adapter");
    }
    let source = std::fs::read_to_string(path).map_err(|_| "adapter_not_installed")?;
    if source.len() > 512 * 1024 {
        return Err("adapter_too_large");
    }
    let start = source.find("/* @meta").ok_or("invalid_adapter")?;
    let end = source[start..].find("*/").ok_or("invalid_adapter")? + start;
    let meta: Value =
        serde_json::from_str(source[start + 8..end].trim()).map_err(|_| "invalid_adapter")?;
    let body = source[end + 2..].trim().to_string();
    let mut fingerprint = serde_json::to_vec(&meta).map_err(|_| "invalid_adapter")?;
    fingerprint.extend(body.as_bytes());
    Ok((meta, body, hex::encode(Sha256::digest(fingerprint))))
}
fn host(meta: &Value) -> Result<String, &'static str> {
    let value = meta["domain"].as_str().ok_or("invalid_domain")?;
    let url = url::Url::parse(
        if value.contains("://") {
            value.to_string()
        } else {
            format!("https://{value}")
        }
        .as_str(),
    )
    .map_err(|_| "invalid_domain")?;
    if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
        return Err("invalid_domain");
    }
    Ok(url.host_str().ok_or("invalid_domain")?.to_string())
}
fn read_only(spec: &str, meta: &Value) -> bool {
    let command = spec
        .split('/')
        .next_back()
        .unwrap_or("")
        .split(['-', '_'])
        .next()
        .unwrap_or("");
    meta["readOnly"] == true
        && ![
            "delete",
            "remove",
            "send",
            "post",
            "publish",
            "update",
            "upload",
            "buy",
            "pay",
            "create",
            "reply",
            "like",
            "follow",
            "subscribe",
        ]
        .contains(&command)
}

fn redact(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .filter(|(k, _)| {
                    ![
                        "cookie",
                        "cookies",
                        "password",
                        "authorization",
                        "token",
                        "access_token",
                        "refresh_token",
                        "session_key",
                        "secret",
                        "private_key",
                    ]
                    .contains(&k.to_ascii_lowercase().as_str())
                })
                .map(|(k, v)| (k, redact(v)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
        Value::String(s) if s.starts_with("eyJ") && s.split('.').count() == 3 => {
            json!("[credential hidden]")
        }
        other => other,
    }
}

/// All errors are codes; no child stdout/stderr or keys enter daemon logs.
pub fn handle(
    file: &Path,
    sites: &Path,
    raw: &Value,
    chrome_bin: &Path,
) -> Result<Value, &'static str> {
    let scope_id = raw["scope"].as_str().ok_or("invalid_request")?;
    let scopes = read_scopes(file)?;
    let scope = scopes["scopes"]
        .as_array()
        .ok_or("invalid_scope_file")?
        .iter()
        .find(|s| s["id"] == scope_id)
        .ok_or("chrome_scope_not_granted")?;
    if scope["expires"].as_u64().unwrap_or(0) < now() {
        return Err("chrome_scope_expired");
    }
    let key: [u8; 32] = STANDARD
        .decode(scope["key"].as_str().ok_or("invalid_scope")?)
        .map_err(|_| "invalid_scope")?
        .try_into()
        .map_err(|_| "invalid_scope")?;
    let request = open_message(
        raw["sealed"].as_str().ok_or("invalid_request")?,
        &key,
        format!("phone-use/chrome/request/{scope_id}").as_bytes(),
    )?;
    let id = request["id"].as_str().ok_or("invalid_request")?;
    if id.is_empty()
        || id.len() > 100
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("invalid_request_id");
    }
    let sent = request["at"]
        .as_u64()
        .or_else(|| {
            request["at"]
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0 && value.fract() == 0.0)
                .map(|value| value as u64)
        })
        .ok_or("invalid_request")?;
    if now().abs_diff(sent) > 90 {
        return Err("request_expired");
    }
    let domains: Vec<String> = scope["domains"]
        .as_array()
        .ok_or("invalid_scope")?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let profile = scope["profile"].as_str().ok_or("invalid_scope")?;
    let response = match request["operation"].as_str() {
        Some("catalog") => {
            let commands = catalog(sites, &domains)?
                .into_iter()
                .filter(|v| scope["pins"][v["spec"].as_str().unwrap()] == v["fingerprint"])
                .collect::<Vec<_>>();
            json!({"ok":true,"profile":profile,"domains":domains,"writes":scope["writes"],"catalog":{"chrome_use_sites":1,"adapters":commands}})
        }
        Some("run") => {
            let spec = request["spec"].as_str().ok_or("invalid_request")?;
            let (meta, _body, fingerprint) = adapter(sites, spec)?;
            if request["fingerprint"] != fingerprint || scope["pins"][spec] != fingerprint {
                return Err("adapter_changed_reconfirm_required");
            }
            if !allowed(&host(&meta)?, &domains) {
                return Err("site_outside_granted_scope");
            }
            let readonly = read_only(spec, &meta);
            if !readonly && (scope["writes"] != true || request["confirmed"] != true) {
                return Err("write_requires_explicit_confirmation");
            }
            let empty = serde_json::Map::new();
            let definitions = meta["args"].as_object().unwrap_or(&empty);
            let args = request["arguments"]
                .as_object()
                .ok_or("invalid_arguments")?;
            if args.len() > 30 || args.keys().any(|k| !definitions.contains_key(k)) {
                return Err("invalid_arguments");
            }
            for (name, definition) in definitions {
                if definition["required"] == true
                    && args
                        .get(name)
                        .is_none_or(|v| v.is_null() || v.as_str() == Some(""))
                {
                    return Err("required_argument_missing");
                }
                if definition["type"] == "file"
                    && args.get(name).is_some_and(|value| !value.is_null())
                {
                    return Err("file_selection_must_happen_on_the_computer");
                }
            }
            for (name, value) in args {
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                    || value.to_string().len() > 32000
                {
                    return Err("invalid_arguments");
                }
            }
            let _lock = scope_lock(&file.with_extension("execution-lock"))?;
            let root = file
                .parent()
                .ok_or("scope_unavailable")?
                .join("chrome-request-records");
            let record = root.join(format!(
                "{}.json",
                hex::encode(Sha256::digest(format!("{scope_id}:{id}")))
            ));
            let mut semantics = request.clone();
            semantics
                .as_object_mut()
                .ok_or("invalid_request")?
                .remove("at");
            let body_hash = hex::encode(Sha256::digest(
                serde_json::to_vec(&semantics).map_err(|_| "invalid_request")?,
            ));
            let aad = format!("phone-use/chrome/record/{scope_id}/{id}");
            if record.exists() {
                let encrypted =
                    std::fs::read_to_string(&record).map_err(|_| "record_unavailable")?;
                let previous = open_message(&encrypted, &key, aad.as_bytes())?;
                if previous["hash"] != body_hash {
                    return Err("request_id_reused_for_other_arguments");
                }
                previous["result"].clone()
            } else {
                // Persist uncertainty before spawning Chrome. A crash/retry sees
                // this record and never starts the same operation again.
                let pending =
                    json!({"ok":false,"error":"operation_outcome_unknown","replay_safe":false});
                private_write(
                    &record,
                    seal_message(
                        &json!({"hash":body_hash,"result":pending}),
                        &key,
                        aad.as_bytes(),
                    )?
                    .as_bytes(),
                )?;
                let mut command = Command::new(chrome_bin);
                command.args([
                    "--session",
                    &format!("phone-use-{scope_id}"),
                    "--profile",
                    profile,
                    "site",
                    spec,
                    "--json",
                    "--",
                ]);
                for (name, value) in args {
                    if !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                    {
                        return Err("invalid_arguments");
                    }
                    let raw = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    // chrome-use accepts @file/@- for any text argument. Phone
                    // arguments are literal text, never permission to read Mac files.
                    let literal = if raw.starts_with('@') {
                        format!("\\{raw}")
                    } else {
                        raw
                    };
                    command.arg(format!("--{name}")).arg(literal);
                }
                command.env("AGENT_BROWSER_SITES_NO_OPENCLI", "1");
                command.stdin(Stdio::null()).stderr(Stdio::null());
                let result = match bounded_output(&mut command) {
                    Ok(output) if output.0 && output.1.len() <= 2 * 1024 * 1024 => {
                        serde_json::from_slice::<Value>(&output.1)
                            .map(redact)
                            .map(|data| json!({"ok":true,"data":data}))
                            .unwrap_or_else(|_| pending.clone())
                    }
                    _ => {
                        json!({"ok":false,"error":if readonly {"chrome_site_failed"} else {"operation_outcome_unknown"},"replay_safe":readonly})
                    }
                };
                private_write(
                    &record,
                    seal_message(
                        &json!({"hash":body_hash,"result":result}),
                        &key,
                        aad.as_bytes(),
                    )?
                    .as_bytes(),
                )?;
                result
            }
        }
        _ => return Err("unsupported_operation"),
    };
    Ok(
        json!({"ok":true,"scope":scope_id,"id":id,"sealed":seal_message(&response,&key,format!("phone-use/chrome/response/{scope_id}/{id}").as_bytes())?}),
    )
}

fn scope_lock(file: &Path) -> Result<std::fs::File, &'static str> {
    use fs2::FileExt;
    let path = file.with_extension("lock");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|_| "scope_unavailable")?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(path).map_err(|_| "scope_unavailable")?;
    lock.try_lock_exclusive()
        .map_err(|_| "chrome_operation_already_running")?;
    Ok(lock)
}
fn catalog(sites: &Path, domains: &[String]) -> Result<Vec<Value>, &'static str> {
    let mut commands = vec![];
    for folder in std::fs::read_dir(sites).map_err(|_| "chrome_use_sites_not_installed")? {
        let folder = folder.map_err(|_| "catalog_unavailable")?.path();
        if !folder.is_dir() || folder.is_symlink() {
            continue;
        }
        for entry in std::fs::read_dir(&folder).map_err(|_| "catalog_unavailable")? {
            let path = entry.map_err(|_| "catalog_unavailable")?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("js") {
                continue;
            }
            let spec = format!(
                "{}/{}",
                folder.file_name().unwrap().to_string_lossy(),
                path.file_stem().unwrap().to_string_lossy()
            );
            if let Ok((meta, body, fingerprint)) = adapter(sites, &spec) {
                if host(&meta).ok().is_some_and(|h| allowed(&h, domains)) {
                    commands.push(json!({"spec":spec,"metaJSON":serde_json::to_string(&meta).map_err(|_|"invalid_adapter")?,"meta":meta,"source":body,"sha256":hex::encode(Sha256::digest(body.as_bytes())),"fingerprint":fingerprint,"origin":"paired chrome-use catalog"}));
                    if commands.len() > 2000 {
                        return Err("catalog_too_large");
                    }
                }
            }
        }
    }
    Ok(commands)
}
fn bounded_output(command: &mut Command) -> Result<(bool, Vec<u8>), &'static str> {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| "chrome_unavailable")?;
    let stdout = child.stdout.take().ok_or("chrome_unavailable")?;
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut data = vec![];
        let result = stdout
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map(|_| data);
        let _ = send.send(result);
    });
    let start = Instant::now();
    let mut captured = None;
    loop {
        if captured.is_none() {
            if let Ok(result) = receive.try_recv() {
                match result {
                    Ok(data) if data.len() <= 2 * 1024 * 1024 => captured = Some(data),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("chrome_output_limit");
                    }
                }
            }
        }
        if let Some(status) = child.try_wait().map_err(|_| "chrome_unavailable")? {
            let data = match captured {
                Some(data) => data,
                None => receive
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| "chrome_output_unavailable")?
                    .map_err(|_| "chrome_output_unavailable")?,
            };
            return Ok((status.success(), data));
        }
        if start.elapsed() > Duration::from_secs(45) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("chrome_timeout");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        sites: PathBuf,
        file: PathBuf,
        scope: String,
        key: [u8; 32],
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ipu-chrome-{}",
                hex::encode(rand::random::<[u8; 12]>())
            ));
            let sites = root.join("sites");
            std::fs::create_dir_all(sites.join("example")).unwrap();
            std::fs::write(sites.join("example/read.js"), "/* @meta {\"domain\":\"example.com\",\"readOnly\":true,\"args\":{\"text\":{\"type\":\"string\"}}} */\nasync function(args) { return {ok:true}; }").unwrap();
            let file = root.join("scopes.json");
            let link = grant(
                &file,
                &sites,
                "Default",
                &["example.com".into()],
                false,
                "http://127.0.0.1:45929",
            )
            .unwrap();
            let link = url::Url::parse(&link).unwrap();
            let query = link
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            Self {
                root,
                sites,
                file,
                scope: query["s"].to_string(),
                key: STANDARD
                    .decode(query["k"].as_bytes())
                    .unwrap()
                    .try_into()
                    .unwrap(),
            }
        }
        fn raw(&self, request: Value) -> Value {
            json!({"scope":self.scope,"sealed":seal_message(&request,&self.key,format!("phone-use/chrome/request/{}",self.scope).as_bytes()).unwrap()})
        }
        fn run_request(&self) -> Value {
            let (_, _, hash) = adapter(&self.sites, "example/read").unwrap();
            json!({"id":"fixture-request","at":now(),"operation":"run","spec":"example/read","fingerprint":hash,"arguments":{}})
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn scope_authentication_and_pins() {
        let f = Fixture::new();
        let raw = f.raw(json!({"id":"catalog","at":now(),"operation":"catalog"}));
        assert!(handle(&f.file, &f.sites, &raw, Path::new("unused")).is_ok());
        let mut bad = raw.clone();
        bad["sealed"] = json!(STANDARD.encode([0u8; 40]));
        assert_eq!(
            handle(&f.file, &f.sites, &bad, Path::new("unused")).unwrap_err(),
            "authentication_failed"
        );
        std::fs::write(f.sites.join("example/read.js"),"/* @meta {\"domain\":\"example.com\",\"readOnly\":true} */ async function(args) {return 'changed';}").unwrap();
        let changed = f.raw(f.run_request());
        assert_eq!(
            handle(&f.file, &f.sites, &changed, Path::new("unused")).unwrap_err(),
            "adapter_changed_reconfirm_required"
        );
        revoke(&f.file, &f.scope).unwrap();
        assert_eq!(
            handle(&f.file, &f.sites, &raw, Path::new("unused")).unwrap_err(),
            "chrome_scope_not_granted"
        );
    }
    #[cfg(unix)]
    #[test]
    fn text_arguments_never_expand_mac_files() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let bin = f.root.join("chrome-stub");
        let argv = f.root.join("argument.txt");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{10}}\" > '{}'\nprintf '{{\"ok\":true}}'\n",
                argv.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut request = f.run_request();
        request["arguments"] = json!({"text":"@/tmp/synthetic-file-never-read"});
        request["at"] = json!(now() as f64);
        let raw = f.raw(request);
        handle(&f.file, &f.sites, &raw, &bin).unwrap();
        assert_eq!(
            std::fs::read_to_string(&argv).unwrap(),
            "\\@/tmp/synthetic-file-never-read"
        );
        let reply = handle(
            &f.file,
            &f.sites,
            &f.raw(json!({"id":"catalog","at":now(),"operation":"catalog"})),
            &bin,
        )
        .unwrap();
        let catalog = open_message(
            reply["sealed"].as_str().unwrap(),
            &f.key,
            format!("phone-use/chrome/response/{}/catalog", f.scope).as_bytes(),
        )
        .unwrap();
        let adapter = &catalog["catalog"]["adapters"][0];
        assert_eq!(
            serde_json::from_str::<Value>(adapter["metaJSON"].as_str().unwrap()).unwrap(),
            adapter["meta"]
        );
    }

    #[test]
    fn write_requires_scope_and_confirmation() {
        let f = Fixture::new();
        // Malicious metadata cannot make an obvious write command read-only.
        assert!(!read_only("example/send", &json!({"readOnly":true})));
        assert!(grant(
            &f.file,
            &f.sites,
            "../../Default",
            &["example.com".into()],
            false,
            "http://localhost"
        )
        .is_err());
        assert!(grant(
            &f.file,
            &f.sites,
            "Default",
            &["com".into()],
            false,
            "http://localhost"
        )
        .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn persisted_result_prevents_duplicate_execution_and_reused_id() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let bin = f.root.join("chrome-stub");
        let counter = f.root.join("count");
        std::fs::write(&bin,format!("#!/bin/sh\nprintf x >> '{}'\nprintf '{{\"title\":\"synthetic\",\"token\":\"hidden\"}}'\n",counter.display())).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let req = f.run_request();
        let raw = f.raw(req.clone());
        let first = handle(&f.file, &f.sites, &raw, &bin).unwrap();
        let result = open_message(
            first["sealed"].as_str().unwrap(),
            &f.key,
            format!("phone-use/chrome/response/{}/fixture-request", f.scope).as_bytes(),
        )
        .unwrap();
        assert_eq!(result["data"], json!({"title":"synthetic"}));
        handle(&f.file, &f.sites, &raw, &bin).unwrap();
        assert_eq!(std::fs::read(&counter).unwrap(), b"x");
        let mut changed = req;
        changed["confirmed"] = json!(true);
        assert_eq!(
            handle(&f.file, &f.sites, &f.raw(changed), &bin).unwrap_err(),
            "request_id_reused_for_other_arguments"
        );
        let lock = scope_lock(&f.file.with_extension("execution-lock")).unwrap();
        assert_eq!(
            handle(&f.file, &f.sites, &raw, &bin).unwrap_err(),
            "chrome_operation_already_running"
        );
        drop(lock);
    }
}
