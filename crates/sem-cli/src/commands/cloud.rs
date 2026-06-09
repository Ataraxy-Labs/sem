use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use colored::Colorize;
use serde::{Deserialize, Serialize};

const DEFAULT_ENDPOINT: &str = "https://api.sem.sh";
const GITHUB_CLIENT_ID: &str = "Ov23liYourClientIdHere"; // TODO: replace with real OAuth App client ID

#[derive(Serialize, Deserialize)]
pub struct CloudCredentials {
    pub api_key: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

fn credentials_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("credentials.json"))
}

fn save_credentials(creds: &CloudCredentials) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = credentials_path().ok_or("Could not determine home directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(creds)?;
    fs::write(&path, json)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(path)
}

/// Load stored cloud credentials, if any.
#[allow(dead_code)]
pub fn load_credentials() -> Option<CloudCredentials> {
    let path = credentials_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

// --- sem login ---

pub fn login(
    api_key: Option<String>,
    endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let key = match api_key {
        Some(k) => k,
        None => {
            eprint!("{}", "Enter your API key: ".bold());
            io::stderr().flush()?;
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                return Err("No API key provided. Use `sem login --github` to log in with GitHub.".into());
            }
            trimmed
        }
    };

    if !key.starts_with("sk_live_") {
        eprintln!(
            "{} Key doesn't start with sk_live_ — are you sure this is correct?",
            "warning:".yellow().bold()
        );
    }

    let ep = endpoint.unwrap_or_else(default_endpoint);
    let creds = CloudCredentials {
        api_key: key,
        endpoint: ep.clone(),
    };

    let path = save_credentials(&creds)?;
    println!("{} Logged in to {}", "ok".green().bold(), ep);
    println!("  Credentials saved to {}", path.display());

    Ok(())
}

// --- sem login --github ---

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    key: String,
}

pub fn login_github(endpoint: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let ep = endpoint.unwrap_or_else(default_endpoint);
    let client_id =
        std::env::var("SEM_GITHUB_CLIENT_ID").unwrap_or_else(|_| GITHUB_CLIENT_ID.into());

    // Step 1: Request device code from GitHub
    let device_resp: DeviceCodeResponse = ureq::post("https://github.com/login/device/code")
        .set("Accept", "application/json")
        .send_form(&[("client_id", &client_id), ("scope", &"user:email".to_string())])?
        .into_json()?;

    let interval = Duration::from_secs(device_resp.interval.unwrap_or(5));

    // Step 2: Show code to user
    println!();
    println!(
        "  Open {} in your browser",
        device_resp.verification_uri.bold()
    );
    println!("  and enter code: {}", device_resp.user_code.cyan().bold());
    println!();

    // Try to open the browser automatically
    let _ = open_url(&device_resp.verification_uri);

    eprint!("{}", "Waiting for authorization...".dimmed());
    io::stderr().flush()?;

    // Step 3: Poll for token
    let access_token = loop {
        thread::sleep(interval);

        let resp: TokenResponse =
            ureq::post("https://github.com/login/oauth/access_token")
                .set("Accept", "application/json")
                .send_form(&[
                    ("client_id", client_id.as_str()),
                    ("device_code", &device_resp.device_code),
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:device_code",
                    ),
                ])?
                .into_json()?;

        match (resp.access_token, resp.error.as_deref()) {
            (Some(token), _) => break token,
            (_, Some("authorization_pending")) => {
                eprint!(".");
                io::stderr().flush()?;
                continue;
            }
            (_, Some("slow_down")) => {
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            (_, Some("expired_token")) => {
                eprintln!();
                return Err("Device code expired. Please try again.".into());
            }
            (_, Some("access_denied")) => {
                eprintln!();
                return Err("Authorization denied.".into());
            }
            (_, Some(err)) => {
                eprintln!();
                return Err(format!("GitHub error: {err}").into());
            }
            _ => continue,
        }
    };
    eprintln!(" {}", "authorized".green());

    // Step 4: Exchange GitHub token for sem API key
    let exchange_url = format!("{}/v1/auth/github", ep.trim_end_matches('/'));
    let exchange_resp: ExchangeResponse = ureq::post(&exchange_url)
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({ "github_token": access_token }))?
        .into_json()?;

    let creds = CloudCredentials {
        api_key: exchange_resp.key,
        endpoint: ep.clone(),
    };

    let path = save_credentials(&creds)?;
    println!("{} Logged in to {} via GitHub", "ok".green().bold(), ep);
    println!("  Credentials saved to {}", path.display());

    Ok(())
}

fn open_url(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn()?;
    }
    Ok(())
}

// --- sem logout ---

pub fn logout() -> Result<(), Box<dyn std::error::Error>> {
    let path = credentials_path().ok_or("Could not determine home directory")?;

    if path.exists() {
        fs::remove_file(&path)?;
        println!(
            "{} Logged out — credentials removed",
            "ok".green().bold()
        );
    } else {
        println!(
            "{} No credentials found — already logged out",
            "ok".green().bold()
        );
    }

    Ok(())
}

// --- sem whoami ---

pub fn whoami() -> Result<(), Box<dyn std::error::Error>> {
    let creds = load_credentials().ok_or("Not logged in. Run: sem login")?;

    // Mask the key: show prefix + last 4
    let masked = if creds.api_key.len() > 16 {
        format!(
            "{}...{}",
            &creds.api_key[..12],
            &creds.api_key[creds.api_key.len() - 4..]
        )
    } else {
        creds.api_key.clone()
    };

    println!("{} {}", "Endpoint:".bold(), creds.endpoint);
    println!("{} {}", "API Key: ".bold(), masked);

    Ok(())
}
