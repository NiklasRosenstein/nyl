//! Real-tool prerequisites and an isolated local registry for tier 2.
use std::process::Command;

pub fn require_tools() -> Result<(), String> {
    for (program, args) in [
        ("tofu", vec!["version", "-json"]),
        ("docker", vec!["buildx", "version"]),
    ] {
        let output = Command::new(program)
            .args(&args)
            .output()
            .map_err(|error| format!("tier 2 requires {program}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "tier 2 requires {program} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(())
}

/// Each scenario owns its registry, its anonymous port, and its cleanup.
pub struct Registry {
    id: String,
    pub address: String,
}
impl Registry {
    pub fn new() -> Result<Self, String> {
        require_tools()?;
        let output = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--pull=never",
                "--rm",
                "--publish",
                "127.0.0.1::5000",
                "registry:2",
            ])
            .output()
            .map_err(|error| format!("start tier-2 registry: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "tier 2 requires a Docker daemon and registry:2: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let mut registry = Self {
            id,
            address: String::new(),
        };
        let output = Command::new("docker")
            .args(["port", &registry.id, "5000/tcp"])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "read tier-2 registry port: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        registry.address = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|line| line.starts_with("127.0.0.1:"))
            .ok_or("registry has no loopback port")?
            .trim()
            .to_owned();
        registry.wait_ready()?;
        Ok(registry)
    }
    fn wait_ready(&self) -> Result<(), String> {
        use std::io::{Read, Write};
        for _ in 0..100 {
            if let Ok(mut stream) = std::net::TcpStream::connect(&self.address) {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(100)));
                let request = format!("GET /v2/ HTTP/1.0\r\nHost: {}\r\n\r\n", self.address);
                if stream.write_all(request.as_bytes()).is_ok() {
                    let mut response = [0_u8; 128];
                    if let Ok(count) = stream.read(&mut response) {
                        if String::from_utf8_lossy(&response[..count])
                            .lines()
                            .next()
                            .is_some_and(|line| line.contains(" 200 "))
                        {
                            return Ok(());
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Err("tier-2 registry did not become ready".to_owned())
    }
}
impl Drop for Registry {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "--force", &self.id]).output();
    }
}
