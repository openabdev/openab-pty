//! The image-provided startup hook.
//!
//! The runtime knows nothing about any coding CLI's config format, and should
//! not: there are a dozen image variants and every vendor shapes its MCP config
//! differently. What *does* know is the image — each variant is built for one
//! CLI — so an image may ship an executable that the runtime runs exactly once,
//! **after seeding and binding, before serving**. The image's own script then
//! wires whatever it knows into the CLI's config (today: the tools MCP, #39).
//!
//! Why the runtime runs it rather than the entrypoint: seeding happens here, in
//! the runtime, and a seed archive may well carry the very config file the hook
//! edits. Run before seeding, the hook's edit would be overwritten. Run after the
//! tools listener is bound, the hook is handed the address the runtime actually
//! bound — the same one sessions get in `OPENAB_TOOLS_MCP_ENDPOINT` — rather than
//! re-deriving it from an environment variable that could disagree.
//!
//! The hook is argv-supplied (`--startup-hook`), not a projection key: it is a
//! property of the image, set by the image's entrypoint, not an operator knob.
//!
//! ## What the hook is handed
//!
//! **The session environment allowlist, not the runtime's environment.** The
//! hook is image code, but it reads and writes `$HOME`, which is the shared
//! workspace every session can write to, and runs tools (jq) that load
//! configuration from there. Anything in its environment is one planted file
//! away from landing in a config the next session reads. So it gets exactly what
//! a session shell gets ([`ENV_ALLOWLIST`] / [`ENV_ALLOWLIST_PREFIXES`]) — never
//! `PTY_ADMIN_HASH`, a forwarded key, or an ECS credentials URI — plus
//! [`TOOLS_LISTEN_ENV`] when the tools plane is on.
//!
//! ## Teardown
//!
//! The hook leads its own process group. When it exits or times out, the whole
//! group is killed and every member that was reparented to this process (the
//! runtime is the child subreaper) is reaped, so an orphaned `jq` or a `cat`
//! hung on a network filesystem neither outlives the hook nor lingers as a
//! zombie. A child that leaves the group (`setsid`) escapes this, as it would
//! escape a session's teardown; the hook is image code and does not do that.
//!
//! ## Contract
//!
//! Best-effort. A failure, a non-zero exit or a timeout is logged and serving
//! continues, because everything a hook wires up has a documented manual path,
//! and a host that refuses to serve over a convenience is worse than one that
//! serves without it.

use crate::session::{ENV_ALLOWLIST, ENV_ALLOWLIST_PREFIXES};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a hook may run before its process group is killed.
pub const STARTUP_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// The bound tools listener (`host:port`, IPv6 bracketed), set only when the
/// tools plane is on. Authoritative: it is the runtime's own bound address.
pub const TOOLS_LISTEN_ENV: &str = "OPENAB_PTY_TOOLS_LISTEN";

const POLL: Duration = Duration::from_millis(20);
/// How long to keep reaping a killed group's reparented members.
const REAP_GRACE: Duration = Duration::from_secs(1);

/// What one hook run came to — logged by [`run_startup_hook`], returned for tests.
#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Succeeded,
    Failed(Option<i32>),
    TimedOut,
    CouldNotStart(String),
    WaitFailed(String),
}

/// The hook's environment: the session allowlist drawn from `source`, plus the
/// bound tools address when the tools plane is on. Nothing else.
pub fn hook_env<I>(source: I, tools_listen: Option<&str>) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut env: Vec<(String, String)> = source
        .into_iter()
        .filter(|(key, _)| {
            ENV_ALLOWLIST.contains(&key.as_str())
                || ENV_ALLOWLIST_PREFIXES
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
        })
        .collect();
    if let Some(listen) = tools_listen {
        env.push((TOOLS_LISTEN_ENV.to_string(), listen.to_string()));
    }
    env
}

/// Run `hook` with `env` (and nothing else) and the runtime's stdio, wait up to
/// `timeout`, tear its process group down, and never fail: the outcome is
/// logged and returned.
pub fn run_startup_hook(hook: &Path, env: &[(String, String)], timeout: Duration) -> HookOutcome {
    let outcome = run(hook, env, timeout);
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
            "startup hook timed out and its process group was killed; serving anyway"
        ),
        HookOutcome::CouldNotStart(error) => tracing::warn!(
            hook = %hook_display,
            %error,
            "startup hook could not be started; serving anyway"
        ),
        HookOutcome::WaitFailed(error) => tracing::warn!(
            hook = %hook_display,
            %error,
            "waiting on the startup hook failed; its group was killed; serving anyway"
        ),
    }
    outcome
}

fn run(hook: &Path, env: &[(String, String)], timeout: Duration) -> HookOutcome {
    let mut command = Command::new(hook);
    command
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group, led by the hook, so teardown can address all of it.
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return HookOutcome::CouldNotStart(error.to_string()),
    };
    let pgid = child.id();
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break HookOutcome::Succeeded,
            Ok(Some(status)) => break HookOutcome::Failed(status.code()),
            Ok(None) if Instant::now() >= deadline => break HookOutcome::TimedOut,
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => break HookOutcome::WaitFailed(error.to_string()),
        }
    };
    // Always, not only on timeout: a hook that exited 0 can still have left a
    // background member behind.
    kill_group(pgid);
    let _ = child.kill();
    let _ = child.wait();
    reap_group(pgid);
    outcome
}

#[cfg(unix)]
fn kill_group(pgid: u32) {
    // SAFETY: killpg has no memory-safety preconditions. ESRCH (the group is
    // already empty) is the common, harmless result.
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: u32) {}

/// Reap members of the killed group that were reparented to this process (the
/// runtime is the child subreaper), until none are left or the grace runs out.
#[cfg(unix)]
fn reap_group(pgid: u32) {
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes only into `status`. `-pgid` restricts it to
        // this hook's group, so it can never reap a session child.
        let reaped = unsafe { libc::waitpid(-(pgid as libc::pid_t), &mut status, libc::WNOHANG) };
        if reaped > 0 {
            continue;
        }
        // -1: no child of ours left in the group (ECHILD). 0: members still
        // exiting — wait briefly for them to be reparented and die.
        if reaped < 0 || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(not(unix))]
fn reap_group(_pgid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Whether `pid` has stopped running. A killed process can linger as a
    /// zombie until whoever inherited it reaps it — in a test process that is
    /// init, not us — and `kill(pid, 0)` still succeeds on a zombie, so wait a
    /// little and count a zombie as gone.
    fn gone(pid: libc::pid_t) -> bool {
        for _ in 0..100 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit(')')
                        .next()
                        .map(|rest| rest.trim_start().starts_with('Z'))
                })
                .unwrap_or(true)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn read_pid(path: &Path) -> libc::pid_t {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("hook.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `run_startup_hook`, retried on ETXTBSY. Tests write their hook script
    /// and exec it at once while other test threads fork; a fork that lands
    /// between the write's open and close holds the file open for writing, and
    /// exec then fails with "Text file busy". A production hook is a file in the
    /// image, never written by this process, so this is a test-only artefact.
    fn run_hook(hook: &Path, env: &[(String, String)], timeout: Duration) -> HookOutcome {
        for _ in 0..100 {
            match run_startup_hook(hook, env, timeout) {
                HookOutcome::CouldNotStart(error) if error.contains("os error 26") => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                outcome => return outcome,
            }
        }
        panic!("hook stayed ETXTBSY");
    }

    fn base_env() -> Vec<(String, String)> {
        hook_env(std::env::vars(), None)
    }

    #[test]
    fn a_zero_exit_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let hook = script(dir.path(), "exit 0");
        assert_eq!(
            run_hook(&hook, &base_env(), STARTUP_HOOK_TIMEOUT),
            HookOutcome::Succeeded
        );
    }

    #[test]
    fn a_non_zero_exit_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let hook = script(dir.path(), "exit 3");
        assert_eq!(
            run_hook(&hook, &base_env(), STARTUP_HOOK_TIMEOUT),
            HookOutcome::Failed(Some(3))
        );
    }

    #[test]
    fn a_hung_hook_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let hook = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        assert_eq!(
            run_hook(&hook, &base_env(), Duration::from_millis(200)),
            HookOutcome::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The shape `exec sleep` hides: the hook's own children. Killing only the
    /// direct child would leave `sleep` running; the group kill must not.
    #[test]
    fn a_timeout_kills_the_hooks_children_too() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let hook = script(
            dir.path(),
            &format!("sleep 30 &\necho $! > '{}'\nwait", pidfile.display()),
        );
        // Long enough that the script has certainly written its pidfile before
        // the deadline, even on a loaded macOS runner.
        assert_eq!(
            run_hook(&hook, &base_env(), Duration::from_secs(2)),
            HookOutcome::TimedOut
        );
        assert!(
            gone(read_pid(&pidfile)),
            "the hook's background child survived the group kill"
        );
    }

    #[test]
    fn a_background_member_does_not_outlive_a_successful_hook() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let hook = script(
            dir.path(),
            &format!("sleep 30 &\necho $! > '{}'\nexit 0", pidfile.display()),
        );
        assert_eq!(
            run_hook(&hook, &base_env(), STARTUP_HOOK_TIMEOUT),
            HookOutcome::Succeeded
        );
        assert!(
            gone(read_pid(&pidfile)),
            "a background member outlived the hook"
        );
    }

    #[test]
    fn a_missing_hook_could_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_hook(
            &dir.path().join("absent"),
            &base_env(),
            STARTUP_HOOK_TIMEOUT,
        );
        assert!(
            matches!(outcome, HookOutcome::CouldNotStart(_)),
            "{outcome:?}"
        );
    }

    #[test]
    fn hook_env_is_the_session_allowlist_plus_the_tools_address() {
        let source = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/workspace".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            ("PTY_ADMIN_HASH".to_string(), "sha256:00".to_string()),
            ("PTY_TOOLS_LISTEN".to_string(), "127.0.0.1:1".to_string()),
            ("PTY_FORWARD_KIRO_API_KEY".to_string(), "secret".to_string()),
            (
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI".to_string(),
                "/v2/credentials/x".to_string(),
            ),
            ("OPENAB_SOMETHING".to_string(), "x".to_string()),
        ];
        let env = hook_env(source, Some("127.0.0.1:8091"));
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH", "HOME", "LC_ALL", TOOLS_LISTEN_ENV]);
        assert_eq!(env.last().unwrap().1, "127.0.0.1:8091");
        assert!(!hook_env(Vec::new(), None)
            .iter()
            .any(|(k, _)| k == TOOLS_LISTEN_ENV));
    }

    /// End to end: what the hook process actually sees is `env` and nothing the
    /// runtime inherited.
    #[test]
    fn the_hook_sees_only_what_it_is_handed() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("env.txt");
        let hook = script(dir.path(), &format!("env > '{}'", dump.display()));
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            (TOOLS_LISTEN_ENV.to_string(), "127.0.0.1:8091".to_string()),
        ];
        assert_eq!(
            run_hook(&hook, &env, STARTUP_HOOK_TIMEOUT),
            HookOutcome::Succeeded
        );
        let seen = std::fs::read_to_string(&dump).unwrap();
        assert!(
            seen.contains("OPENAB_PTY_TOOLS_LISTEN=127.0.0.1:8091"),
            "{seen}"
        );
        // The test process has e.g. CARGO_* and RUST_* set; none may leak.
        assert!(!seen.contains("CARGO"), "{seen}");
        assert!(!seen.contains("RUST_"), "{seen}");
    }
}
