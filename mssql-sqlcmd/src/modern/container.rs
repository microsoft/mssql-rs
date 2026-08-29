// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driving a container runtime through its command-line client.
//!
//! go-sqlcmd talks to the Docker daemon over its HTTP API. Shelling out to the
//! `docker` binary instead keeps this to one dependency-free module and works
//! unchanged against Podman, whose CLI is compatible.

use std::process::Command;

/// The runtimes to look for, in the order go-sqlcmd prefers them.
const RUNTIMES: &[&str] = &["docker", "podman"];

pub struct Runtime {
    program: String,
}

impl Runtime {
    /// Finds an installed runtime, or explains that none is.
    pub fn detect() -> Result<Self, String> {
        for program in RUNTIMES {
            if Command::new(program)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                return Ok(Runtime {
                    program: (*program).to_string(),
                });
            }
        }
        Err("no container runtime found. Install Docker or Podman and try again".to_string())
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.program))?;
        if !output.status.success() {
            return Err(format!(
                "{} {} failed: {}",
                self.program,
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn pull(&self, image: &str) -> Result<(), String> {
        self.run(&["pull", image]).map(|_| ())
    }

    /// Starts a detached SQL Server container and returns its id.
    pub fn create_mssql(
        &self,
        image: &str,
        name: &str,
        port: u16,
        password: &str,
        collation: &str,
        hostname: &str,
    ) -> Result<String, String> {
        let ports = format!("{port}:1433");
        // The password reaches the container as an environment variable rather
        // than an argument, so it stays out of the host's process table.
        let mut args = vec![
            "run",
            "-d",
            "--name",
            name,
            "-p",
            &ports,
            "-e",
            "ACCEPT_EULA=Y",
            "-e",
        ];
        let password_env = format!("MSSQL_SA_PASSWORD={password}");
        args.push(&password_env);
        let collation_env = format!("MSSQL_COLLATION={collation}");
        args.push("-e");
        args.push(&collation_env);
        if !hostname.is_empty() {
            args.push("--hostname");
            args.push(hostname);
        }
        args.push(image);
        self.run(&args)
    }

    pub fn start(&self, id: &str) -> Result<(), String> {
        self.run(&["start", id]).map(|_| ())
    }

    pub fn stop(&self, id: &str) -> Result<(), String> {
        self.run(&["stop", id]).map(|_| ())
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.run(&["rm", "--force", id]).map(|_| ())
    }

    /// Whether the container is running right now.
    pub fn is_running(&self, id: &str) -> bool {
        self.run(&["inspect", "-f", "{{.State.Running}}", id])
            .is_ok_and(|text| text == "true")
    }

    /// Waits for SQL Server to log the line that means it is accepting
    /// connections. The server takes several seconds to come up, and a
    /// connection attempt before then fails in a way that looks like a
    /// configuration error.
    pub fn wait_for_log(&self, id: &str, marker: &str, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Ok(logs) = self.run(&["logs", id])
                && logs.contains(marker)
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        false
    }

    /// Tags available for a repository, in the order the registry lists them.
    ///
    /// Reads the registry's public catalogue, which needs no authentication for
    /// MCR. The list runs to hundreds of entries and is paginated, so `Link`
    /// headers are followed to the end.
    pub async fn tags(registry: &str, repo: &str) -> Result<Vec<String>, String> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("cannot create an HTTP client: {e}"))?;
        let mut url = format!("https://{registry}/v2/{repo}/tags/list");
        let mut tags = Vec::new();

        loop {
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("cannot reach {url}: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("{url} returned {}", response.status()));
            }
            let next = next_page(&response, registry);
            let body: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("{url} did not return JSON: {e}"))?;
            if let Some(list) = body.get("tags").and_then(|t| t.as_array()) {
                tags.extend(list.iter().filter_map(|t| t.as_str()).map(str::to_string));
            }
            match next {
                Some(link) => url = link,
                None => break,
            }
        }
        Ok(tags)
    }
}

/// The next page's URL from a `Link: <path>; rel="next"` header, if any.
fn next_page(response: &reqwest::Response, registry: &str) -> Option<String> {
    let link = response
        .headers()
        .get(reqwest::header::LINK)?
        .to_str()
        .ok()?;
    if !link.contains("rel=\"next\"") {
        return None;
    }
    let path = link.split('<').nth(1)?.split('>').next()?;
    Some(format!("https://{registry}{path}"))
}

/// A short random suffix, for names that must not collide.
pub fn unique_suffix() -> Result<String, String> {
    Ok(random_bytes(4)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Generates a password meeting SQL Server's complexity policy.
///
/// Sourced from the OS random number generator: a container reachable on a
/// published port needs a password that cannot be guessed from the time of
/// creation.
pub fn generate_password(length: usize, specials: &str) -> Result<String, String> {
    const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const DIGITS: &[u8] = b"0123456789";

    let length = length.max(8);
    let specials = if specials.is_empty() {
        "!@#$%&*"
    } else {
        specials
    };
    let alphabets: [&[u8]; 4] = [UPPER, LOWER, DIGITS, specials.as_bytes()];

    let bytes = random_bytes(length * 2)?;
    let mut password = String::with_capacity(length);
    // The first four characters cover each required class, so the result always
    // satisfies the policy; the rest are drawn from everything.
    for (index, byte) in bytes.iter().take(length).enumerate() {
        let alphabet = if index < alphabets.len() {
            alphabets[index]
        } else {
            alphabets[(bytes[length + index % length] as usize) % alphabets.len()]
        };
        password.push(alphabet[*byte as usize % alphabet.len()] as char);
    }
    Ok(password)
}

#[cfg(windows)]
fn random_bytes(count: usize) -> Result<Vec<u8>, String> {
    // `RtlGenRandom`, reached through the documented `SystemFunction036` name.
    unsafe extern "system" {
        fn SystemFunction036(buffer: *mut u8, length: u32) -> u8;
    }
    let mut buffer = vec![0u8; count];
    let ok = unsafe { SystemFunction036(buffer.as_mut_ptr(), count as u32) };
    if ok == 0 {
        return Err("the operating system random number generator failed".to_string());
    }
    Ok(buffer)
}

#[cfg(not(windows))]
fn random_bytes(count: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buffer = vec![0u8; count];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buffer))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_password_meets_the_complexity_policy() {
        let password = generate_password(20, "!@#$%&*").unwrap();
        assert_eq!(password.chars().count(), 20);
        assert!(password.chars().any(|c| c.is_ascii_uppercase()));
        assert!(password.chars().any(|c| c.is_ascii_lowercase()));
        assert!(password.chars().any(|c| c.is_ascii_digit()));
        assert!(password.chars().any(|c| "!@#$%&*".contains(c)));
    }

    #[test]
    fn passwords_differ_between_calls() {
        let a = generate_password(32, "!@#$%&*").unwrap();
        let b = generate_password(32, "!@#$%&*").unwrap();
        assert_ne!(a, b, "a predictable password would be worse than none");
    }

    #[test]
    fn a_short_length_is_raised_to_the_minimum() {
        assert_eq!(generate_password(3, "!").unwrap().chars().count(), 8);
    }
}
