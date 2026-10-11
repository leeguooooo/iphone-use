//! Desktop handoff: chrome-use is the cookie/site authority; this program only
//! creates an encrypted, explicitly selected handoff for Phone Use Remote.
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use zeroize::Zeroizing;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Grant a selected Chrome profile/sites for seven days; prints a secret pairing link.
    Grant {
        #[arg(long)]
        profile: String,
        #[arg(long, value_delimiter = ',')]
        sites: Vec<String>,
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        writes: bool,
        #[arg(long)]
        confirm: bool,
    },
    Revoke {
        #[arg(long)]
        scope: String,
    },
    /// Package installed chrome-use site definitions, without browser/session I/O.
    Sites {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Preview by default; --confirm captures only the explicitly selected sites.
    Export {
        #[arg(long)]
        profile: String,
        #[arg(long, value_delimiter = ',', required = true)]
        sites: Vec<String>,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        bookmarks: bool,
        #[arg(long)]
        history: bool,
        #[arg(long)]
        profile_data: Option<PathBuf>,
        #[arg(long)]
        with_sites: bool,
        #[arg(long)]
        confirm: bool,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Convert a cookie-use v1/v2 handoff into another encrypted handoff.
    Repack {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        with_sites: bool,
    },
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("browser-use-bridge: {error}");
        std::process::exit(1);
    }
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("home_unavailable".into())
}
fn default_sites() -> Result<PathBuf, String> {
    Ok(home()?.join(".chrome-use/sites"))
}
fn host_allowed(host: &str, domains: &[String]) -> bool {
    domains
        .iter()
        .any(|d| host == d || host.ends_with(&format!(".{d}")))
}
fn normalized_domains(values: Vec<String>) -> Result<Vec<String>, String> {
    let mut out = vec![];
    for value in values {
        let value = value.trim().trim_start_matches('.').to_ascii_lowercase();
        if value.len() > 253
            || !value.contains('.')
            || value.contains("..")
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        {
            return Err("invalid_site_domain".into());
        }
        if !out.contains(&value) {
            out.push(value);
        }
    }
    if out.is_empty() || out.len() > 30 {
        return Err("select_one_to_thirty_sites".into());
    }
    Ok(out)
}

fn catalog(root: &Path) -> Result<Value, String> {
    let mut adapters = vec![];
    for site in std::fs::read_dir(root).map_err(|_| "chrome_use_sites_not_installed")? {
        let path = site.map_err(|_| "cannot_read_sites")?.path();
        if !path.is_dir() || path.is_symlink() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid_site_name")?;
        for entry in std::fs::read_dir(&path).map_err(|_| "cannot_read_sites")? {
            let file = entry.map_err(|_| "cannot_read_sites")?.path();
            if file.extension().and_then(|s| s.to_str()) != Some("js") || file.is_symlink() {
                continue;
            }
            let source = std::fs::read_to_string(&file).map_err(|_| "cannot_read_adapter")?;
            if source.len() > 512 * 1024 || !source.contains("/* @meta") {
                continue;
            }
            let Some(start) = source.find("/* @meta") else {
                continue;
            };
            let Some(end) = source[start..].find("*/").map(|i| i + start) else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<Value>(source[start + 8..end].trim()) else {
                continue;
            };
            let body = source[end + 2..].trim();
            if !body.starts_with("async function") || meta["domain"].as_str().is_none() {
                continue;
            }
            let command = file
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("invalid_command")?;
            let spec = format!("{name}/{command}");
            adapters.push(json!({"spec":spec,"meta":meta,"source":body,
                "sha256":hex::encode(Sha256::digest(body.as_bytes())),"origin":"chrome-use installed site catalog"}));
            if adapters.len() > 2000 {
                return Err("too_many_adapters".into());
            }
        }
    }
    Ok(json!({"chrome_use_sites":1,"adapters":adapters}))
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "output_already_exists_or_not_writable")?;
    file.write_all(data)
        .map_err(|_| "cannot_write_output".into())
}

fn profile_dir(profile: &str) -> Result<PathBuf, String> {
    if profile != "Default"
        && !(profile.starts_with("Profile ")
            && profile[8..].bytes().all(|b| b.is_ascii_digit())
            && profile.len() > 8)
    {
        return Err("use_a_chrome_profile_directory_id".into());
    }
    let root = home()?.join("Library/Application Support/Google/Chrome");
    let root = std::fs::canonicalize(root).map_err(|_| "chrome_profile_unavailable")?;
    let path =
        std::fs::canonicalize(root.join(profile)).map_err(|_| "chrome_profile_unavailable")?;
    if !path.starts_with(&root) {
        return Err("profile_outside_chrome".into());
    }
    Ok(path)
}

fn collect_bookmarks(value: &Value, domains: &[String], out: &mut Vec<Value>) {
    if let Some(url) = value["url"].as_str() {
        if let Ok(parsed) = url::Url::parse(url) {
            if parsed.scheme() == "https"
                && parsed.host_str().is_some_and(|h| host_allowed(h, domains))
            {
                out.push(json!({"url":url,"title":value["name"].as_str().unwrap_or(""),"source":"chrome-use handoff"}));
            }
        }
    }
    if let Some(children) = value["children"].as_array() {
        for child in children {
            collect_bookmarks(child, domains, out);
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    match args.command {
        Action::Grant {
            profile,
            sites,
            endpoint,
            writes,
            confirm,
        } => {
            let domains = normalized_domains(sites)?;
            profile_dir(&profile)?;
            if !confirm {
                println!(
                    "{}",
                    json!({"preview":true,"profile":profile,"sites":domains,"writes":writes,"endpoint":endpoint,"expires_in_days":7,"next":"Review scope, then use --confirm. Keep the pairing link private."})
                );
                return Ok(());
            }
            let link = ipu_browser_session::desktop::grant(
                &ipu_browser_session::desktop::default_scope_file()?,
                &default_sites()?,
                &profile,
                &domains,
                writes,
                &endpoint,
            )?;
            println!("{link}");
        }
        Action::Revoke { scope } => {
            ipu_browser_session::desktop::revoke(
                &ipu_browser_session::desktop::default_scope_file()?,
                &scope,
            )?;
            println!("{}", json!({"ok":true}));
        }

        Action::Sites { root, out } => {
            let data = catalog(&root.unwrap_or(default_sites()?))?;
            write_private(
                &out,
                &serde_json::to_vec(&data).map_err(|_| "invalid_catalog")?,
            )?;
            println!(
                "{}",
                json!({"ok":true,"adapters":data["adapters"].as_array().map(Vec::len),"out":out})
            );
        }
        Action::Export {
            profile,
            sites,
            out,
            bookmarks,
            history,
            profile_data,
            with_sites,
            confirm,
            password_stdin,
        } => {
            let domains = normalized_domains(sites)?;
            let dir = profile_dir(&profile)?;
            if !confirm {
                println!(
                    "{}",
                    json!({"preview":true,"profile":profile,"sites":domains,"bookmarks":bookmarks,"history":history,"personal_data":profile_data.is_some(),"site_catalog":with_sites,"destination":"Phone Use Remote encrypted import","captured":false,"next":"Review this scope, then run with --confirm"})
                );
                return Ok(());
            }
            let password = Zeroizing::new(if password_stdin {
                let mut s = String::new();
                std::io::stdin()
                    .read_line(&mut s)
                    .map_err(|_| "password_input_failed")?;
                s.trim_end_matches(['\r', '\n']).to_string()
            } else {
                rpassword::prompt_password("Transfer password (at least 12 characters): ")
                    .map_err(|_| "password_input_failed")?
            });
            if password.chars().count() < 12 {
                return Err("transfer_password_too_short".into());
            }
            let mut cookies = BTreeMap::new();
            for domain in &domains {
                let result = Command::new("chrome-use")
                    .args([
                        "cookies", "export", "--from", &profile, "--domain", domain, "--json",
                    ])
                    .stderr(Stdio::null())
                    .output()
                    .map_err(|_| "chrome_use_unavailable")?;
                if !result.status.success() {
                    return Err("chrome_use_cookie_export_failed_no_cookie_values_logged".into());
                }
                let data: Value = serde_json::from_slice(&result.stdout)
                    .map_err(|_| "invalid_chrome_use_response")?;
                let list = data["data"]
                    .as_array()
                    .or_else(|| data.as_array())
                    .ok_or("invalid_chrome_use_response")?;
                for c in list {
                    let domain = c["domain"].as_str().unwrap_or("").trim_start_matches('.');
                    if host_allowed(domain, &domains) {
                        cookies
                            .insert(format!("{}|{}|{}", domain, c["path"], c["name"]), c.clone());
                    }
                }
            }
            let mut saved_bookmarks = vec![];
            if bookmarks {
                let data: Value = serde_json::from_slice(
                    &std::fs::read(dir.join("Bookmarks")).map_err(|_| "bookmarks_unavailable")?,
                )
                .map_err(|_| "invalid_bookmarks")?;
                if let Some(roots) = data["roots"].as_object() {
                    for root in roots.values() {
                        collect_bookmarks(root, &domains, &mut saved_bookmarks);
                    }
                }
            }
            let mut saved_history = vec![];
            if history {
                let db = rusqlite::Connection::open_with_flags(
                    dir.join("History"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .map_err(|_| "history_unavailable")?;
                let mut query=db.prepare("SELECT url,title,last_visit_time FROM urls ORDER BY last_visit_time DESC LIMIT 500").map_err(|_| "history_unavailable")?;
                let rows = query
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(|_| "history_unavailable")?;
                for row in rows {
                    let (url, title, time) = row.map_err(|_| "history_unavailable")?;
                    if let Ok(parsed) = url::Url::parse(&url) {
                        if parsed.scheme() == "https"
                            && parsed.host_str().is_some_and(|h| host_allowed(h, &domains))
                        {
                            saved_history.push(json!({"url":url,"title":title,"chrome_time":time}));
                        }
                    }
                }
            }
            let personal = if let Some(path) = profile_data {
                serde_json::from_slice::<Value>(
                    &std::fs::read(path).map_err(|_| "personal_data_unavailable")?,
                )
                .map_err(|_| "invalid_personal_data")?
            } else {
                json!({})
            };
            let allowed = [
                "name",
                "email",
                "phone",
                "address",
                "city",
                "postalCode",
                "country",
            ];
            if personal.as_object().is_none_or(|p| {
                p.iter().any(|(k, v)| {
                    !allowed.contains(&k.as_str()) || v.as_str().is_none_or(|s| s.len() > 2000)
                })
            }) {
                return Err("personal_data_contains_unsupported_fields".into());
            }
            let now = chrono::Utc::now().to_rfc3339();
            let mut payload = json!({"accounts":[{"id":format!("chrome/{profile}"),"site":domains.join(","),"label":profile,"cookies":cookies.into_values().collect::<Vec<_>>(),"created_at":now,"updated_at":now,"status":"unknown"}],
                "browser_data":{"bookmarks":saved_bookmarks,"history":saved_history,"personal":personal}});
            if with_sites {
                payload["site_catalog"] = catalog(&default_sites()?)?;
            }
            let encrypted =
                ipu_browser_session::seal(&payload, password.as_bytes()).map_err(str::to_string)?;
            write_private(&out, &encrypted)?;
            println!(
                "{}",
                json!({"ok":true,"out":out,"accounts":1,"sites":domains,"encrypted":true,"cookie_values_logged":false})
            );
        }
        Action::Repack {
            input,
            out,
            with_sites,
        } => {
            let password = Zeroizing::new(
                rpassword::prompt_password("Existing bundle password: ")
                    .map_err(|_| "password_input_failed")?,
            );
            let mut payload = ipu_browser_session::open(
                &std::fs::read(input).map_err(|_| "bundle_unavailable")?,
                password.as_bytes(),
            )
            .map_err(str::to_string)?;
            if with_sites {
                payload["site_catalog"] = catalog(&default_sites()?)?;
            }
            let encrypted =
                ipu_browser_session::seal(&payload, password.as_bytes()).map_err(str::to_string)?;
            write_private(&out, &encrypted)?;
            println!("{}", json!({"ok":true,"out":out,"encrypted":true}));
        }
    }
    Ok(())
}
