//! The image-provided startup hook.
//!
//! The runtime knows nothing about any coding CLI's config format, and should
//! not: there are a dozen image variants and every vendor shapes its MCP config
//! differently. What *does* know is the image — each variant is built for one
//! CLI — so an image may ship an executable that the runtime runs exactly once,
//! **after seeding and before serving**. The image's own script then wires
//! whatever it knows into the CLI's config (today: the tools MCP, #39).
//!
//! Why the runtime runs it rather than the entrypoint: seeding happens here, in
//! the runtime, and a seed archive may well carry the very config file the hook
//! edits. Run before seeding, the hook's edit would be overwritten. Run here, it
//! layers on top of whatever the seed delivered.
//!
//! The hook is argv-supplied (`--startup-hook`), not a projection key: it is a
//! property of the image, set by the image's entrypoint, not an operator knob.
//!
//! A hook is best-effort by contract. A failure, a non-zero exit or a timeout is
//! logged and serving continues, because everything a hook wires up has a
//! documented manual path, and a host that refuses to serve over a convenience
//! is worse than one that serves without it.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a hook may run before it is killed.
pub const STARTUP_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(20);

/// What one hook run came to — logged by [`run_startup_hook`], returned for tests.
#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Succeeded,
    Failed(Option<i32>),
    TimedOut,
    CouldNotStart(String),
}

/// Run `hook` with the runtime's own environment and stdio, wait up to
/// `timeout`, and never fail: the outcome is logged and returned.
pub fn run_startup_hook(hook: &Path, timeout: Duration) -> HookOutcome {
    let outcome = run(hook, timeout);
    let hook_display = hook.display();
    match &outcome {
        HookOutcome::Succeeded => tracing::info!(hook = %hook_display, "startup hook ran"),
        HookOutcome::Failed(code) => tracing::warn!(
            hook = %hook_display,
            ?code,
            "startup hook failed; serving anyway (what it wires has a manual path)"
        ),
        HookOutcome::TimedOut => tracing::warn!(
            hook = %hook_display,
            ?timeout,
            "startup hook timed out and was killed; serving anyway"
        ),
        HookOutcome::CouldNotStart(error) => tracing::warn!(
            hook = %hook_display,
            %error,
            "startup hook could not be started; serving anyway"
        ),
    }
    outcome
}

fn run(hook: &Path, timeout: Duration) -> HookOutcome {
    let mut child = match Command::new(hook)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return HookOutcome::CouldNotStart(error.to_string()),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return HookOutcome::Succeeded,
            Ok(Some(status)) => return HookOutcome::Failed(status.code()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return HookOutcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return HookOutcome::CouldNotStart(error.to_string());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("hook.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_zero_exit_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let hook = script(dir.path(), "exit 0");
        assert_eq!(
            run_startup_hook(&hook, STARTUP_HOOK_TIMEOUT),
            HookOutcome::Succeeded
        );
    }

    #[test]
    fn a_non_zero_exit_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let hook = script(dir.path(), "exit 3");
        assert_eq!(
            run_startup_hook(&hook, STARTUP_HOOK_TIMEOUT),
            HookOutcome::Failed(Some(3))
        );
    }

    #[test]
    fn a_hung_hook_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let hook = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        assert_eq!(
            run_startup_hook(&hook, Duration::from_millis(200)),
            HookOutcome::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_hook_could_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_startup_hook(&dir.path().join("absent"), STARTUP_HOOK_TIMEOUT);
        assert!(
            matches!(outcome, HookOutcome::CouldNotStart(_)),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_hook_inherits_the_runtime_environment() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("seen");
        // PATH is always set for the test process; the hook must see it.
        let hook = script(
            dir.path(),
            &format!("[ -n \"$PATH\" ] && touch '{}'", marker.display()),
        );
        assert_eq!(
            run_startup_hook(&hook, STARTUP_HOOK_TIMEOUT),
            HookOutcome::Succeeded
        );
        assert!(marker.exists());
    }
}
