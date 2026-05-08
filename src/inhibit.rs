use std::process::{Child, Command, Stdio};

/// Holds an idle/sleep inhibitor lock for as long as the value is alive.
///
/// On Linux/systemd, spawns `systemd-inhibit --what=idle:sleep ... sleep infinity`
/// and kills the child on drop. If `systemd-inhibit` isn't on PATH (non-systemd
/// system, missing package), construction silently no-ops — encoding proceeds,
/// the screensaver just isn't blocked.
pub struct Inhibitor {
    child: Option<Child>,
}

impl Inhibitor {
    pub fn acquire(why: &str) -> Self {
        Self::acquire_with("systemd-inhibit", why)
    }

    fn acquire_with(prog: &str, why: &str) -> Self {
        let child = Command::new(prog)
            .arg("--what=idle:sleep")
            .arg("--who=media-convert")
            .arg(format!("--why={why}"))
            .arg("--mode=block")
            .arg("sleep")
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        Self { child }
    }

    pub fn is_active(&self) -> bool {
        self.child.is_some()
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    fn systemd_inhibit_on_path() -> bool {
        Command::new("systemd-inhibit")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn missing_program_yields_inactive() {
        let inh = Inhibitor::acquire_with("media-convert-no-such-binary-xyz-9f2a", "test");
        assert!(!inh.is_active());
        // Drop must be a no-op when there's no child.
    }

    #[test]
    fn drop_kills_child_process() {
        if !systemd_inhibit_on_path() {
            eprintln!("skipping: systemd-inhibit not available");
            return;
        }
        let pid = {
            let inh = Inhibitor::acquire("media-convert test");
            assert!(inh.is_active(), "expected systemd-inhibit to spawn");
            inh.child.as_ref().expect("child set when active").id()
        };
        // Drop has run. The spawned systemd-inhibit (and its `sleep` grandchild)
        // should disappear from /proc within a moment.
        let proc_path = format!("/proc/{pid}");
        for _ in 0..100 {
            if !Path::new(&proc_path).exists() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("systemd-inhibit pid {pid} still alive after Inhibitor drop");
    }
}
