//! Archive extraction for seeded agent state.
//!
//! ## Vendored code
//!
//! The extraction layer below is vendored from OpenAB, not written here:
//!
//! - source: `openabdev/openab`, `crates/openab-core/src/pre_seed.rs`
//! - upstream commit: `6abb6a3f8113272a1d3369b8228bb8a1fef567be`
//! - upstream file sha256: `c2da4b66a5ad3949a17797f1619592fb0f3e22ad085166c377048643706a2847`
//!
//! It is copied rather than depended on because openab-pty was split out of that
//! monorepo deliberately, and a git dependency back onto it would re-couple the
//! build to every change in a large workspace. Publishing `openab-core` to
//! crates.io for one function was the other option, and commits an internal crate
//! to a public API for less reason.
//!
//! Copying security-relevant code means an upstream fix can fail to arrive here.
//! `.github/workflows/vendor-drift.yml` hashes the upstream file on a schedule and
//! opens an issue when it moves, so that divergence is a notification rather than a
//! silence. When syncing, note that the numbered comments (F1, F2, F5, F6, F9)
//! refer to findings from the review that hardened this code upstream; they are
//! kept so a future reader can find that history.
//!
//! ## Deliberate deviations from upstream
//!
//! 1. **The zip path is removed**, along with the `zip` dependency. Everything that
//!    writes these archives is ours and writes gzipped tar, so a second format is
//!    parsing surface carried for nobody. `extract_and_apply` now *requires* the
//!    gzip magic bytes and errors otherwise, where upstream fell through to zip.
//! 2. **The tarball limits are parameters**, as upstream's zip limits already were.
//!    Without that, the budget tests need a 500 MB fixture to prove anything.
//! 3. **Error text says `seed:`** rather than `seed:`. This runtime has no
//!    hooks, and a message naming a concept that does not exist here would send a
//!    reader looking for config that is not there.
//!
//! There is deliberately no fourth deviation. `% n == 0` was tried in place of
//! `is_multiple_of`, on the theory that the pinned `rust:1-bookworm` builder might
//! predate the intrinsic. Clippy runs with `-D warnings` and rejects the manual
//! form outright, and upstream already ships `is_multiple_of` and builds in that
//! same base -- so the caution cost a CI failure and bought nothing.
//!
//! The S3 side is deliberately *not* vendored. Its orchestration is coupled to
//! OpenAB's `PreSeedConfig`/`OnFailure` types, and this runtime needs different
//! behaviour anyway: a fixed `_shared/` then `<agent>/` layering rather than an
//! arbitrary source list, credentials from the task role, and failures reported
//! through the existing startup sequence.

use std::path::Path;
use std::time::Instant;
use tracing::warn;

/// Cap on total extracted bytes, guarding against a decompression bomb.
const DEFAULT_MAX_EXTRACTED_BYTES: u64 = 500 * 1024 * 1024;

/// Cap on entries in one archive.
const DEFAULT_MAX_FILE_COUNT: usize = 10_000;

/// Extract archive to a temp directory with budget enforcement, then move into target.
/// Requires a gzipped tarball; other formats are rejected on magic bytes.
/// Checks deadline cooperatively before each file operation.
pub fn extract_and_apply(data: &[u8], target: &Path, deadline: Instant) -> anyhow::Result<()> {
    std::fs::create_dir_all(target)?;
    let temp_dir = tempfile::tempdir_in(target)?;

    // Gzip magic, or nothing. Upstream fell through to zip here; this runtime only
    // ever reads archives it wrote, and they are gzipped tar.
    if !data.starts_with(&[0x1f, 0x8b]) {
        anyhow::bail!("seed: archive is not gzipped tar (bad magic bytes)");
    }
    extract_tarball_with_limits(data, temp_dir.path(), deadline)?;

    // Check deadline before applying to target
    if Instant::now() >= deadline {
        anyhow::bail!("seed: timed out before applying to target");
    }

    move_recursive(temp_dir.path(), target, deadline)?;
    Ok(())
}

/// Extract a .tar.gz/.tgz archive with cooperative deadline checks and size budget.
fn extract_tarball_with_limits(data: &[u8], dest: &Path, deadline: Instant) -> anyhow::Result<()> {
    extract_tarball_budgeted(
        data,
        dest,
        deadline,
        DEFAULT_MAX_FILE_COUNT,
        DEFAULT_MAX_EXTRACTED_BYTES,
    )
}

/// Inner extraction with configurable limits, so the budget tests do not need a
/// 500 MB fixture to prove the budget works.
fn extract_tarball_budgeted(
    data: &[u8],
    dest: &Path,
    deadline: Instant,
    max_file_count: usize,
    max_extracted_bytes: u64,
) -> anyhow::Result<()> {
    use flate2::read::GzDecoder;

    let decoder = GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(false);

    let mut file_count: usize = 0;
    let mut total_extracted: u64 = 0;

    for entry in archive.entries()? {
        let mut entry = entry?;

        file_count += 1;
        if file_count > max_file_count {
            anyhow::bail!(
                "seed: tarball contains too many entries ({file_count}, max {max_file_count})"
            );
        }

        // Cooperative deadline check every 10 files
        if file_count.is_multiple_of(10) && Instant::now() >= deadline {
            anyhow::bail!("seed: timed out during tarball extraction at entry {file_count}");
        }

        // Size budget
        total_extracted += entry.size();
        if total_extracted > max_extracted_bytes {
            anyhow::bail!(
                "seed: extracted size exceeds limit ({total_extracted} > {max_extracted_bytes})"
            );
        }

        // F1: Validate symlink/hardlink targets BEFORE writing them to disk.
        // Skipping escaping entries eliminates the TOCTOU window.
        //
        // F9: Hard link targets in tar are relative to the archive root (dest),
        // NOT the link's parent directory. Symlink targets are relative to the
        // symlink's parent. Handle them with different resolution semantics.
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            if let Ok(Some(link_target)) = entry.link_name() {
                if let Ok(entry_path) = entry.path() {
                    let out_path = dest.join(&*entry_path);
                    let escapes = if entry_type.is_hard_link() {
                        // Hard link targets in tar are relative to archive root
                        let resolved = normalize_path(&dest.join(&*link_target));
                        let root_normalized = normalize_path(dest);
                        !resolved.starts_with(&root_normalized)
                    } else {
                        symlink_escapes(dest, &out_path, &link_target)
                    };
                    if escapes {
                        warn!(
                            "seed: skipping link with escaping target: {} -> {}",
                            entry_path.display(),
                            link_target.display()
                        );
                        continue;
                    }
                }
            }
        }

        entry.unpack_in(dest)?;

        // Manually set permissions (strip suid/sgid/sticky, like zip path)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(path) = entry.path() {
                let out_path = dest.join(path);
                if out_path
                    .symlink_metadata()
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false)
                {
                    let mode = entry.header().mode().unwrap_or(0o644) & 0o0777;
                    let _ =
                        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }

    Ok(())
}

/// Check whether a symlink at `link_path` with target `link_target` escapes `root`.
/// Resolves the target relative to the symlink's parent directory and checks
/// whether the normalized result stays within root. This allows relative symlinks
/// with `..` components (e.g., `../aws-cli/v2/current/bin/aws`) as long as they
/// don't escape the extraction root.
///
/// F6: `normalize_path` resolves `..` across the *entire* joined path, including any
/// `..` components in `link_path.parent()` itself (e.g., a tar entry like
/// `a/../b/link`). This makes the containment check robust regardless of how the
/// entry path is structured. (In practice `tar::Entry::path` is already sanitized.)
fn symlink_escapes(root: &Path, link_path: &Path, link_target: &Path) -> bool {
    // Absolute symlinks always escape
    if link_target.is_absolute() {
        return true;
    }
    // Resolve the symlink target relative to the symlink's parent directory
    let parent = link_path.parent().unwrap_or(root);
    let resolved = normalize_path(&parent.join(link_target));
    let root_normalized = normalize_path(root);
    !resolved.starts_with(&root_normalized)
}

/// Normalize a path by resolving `.` and `..` components lexically (no filesystem
/// access, avoiding canonicalize TOCTOU).
///
/// F5: Leading `..` components that would traverse above the path root are dropped
/// rather than preserved. For a pure-relative input like `../../etc`, this yields
/// `etc` (the leading `..` have nothing to pop). Callers in this module always pass
/// absolute paths (the extraction root and entries joined onto it), so a dropped
/// leading `..` cannot produce a false "contained" result — see `symlink_escapes`,
/// where both sides are absolute and compared via component-wise `starts_with`.
fn normalize_path(path: &Path) -> std::path::PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Only pop if there's something to pop and it's not a prefix/root
                if components.last().is_some_and(|c| {
                    !matches!(
                        c,
                        std::path::Component::RootDir | std::path::Component::Prefix(_)
                    )
                }) {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

/// Create a relative symlink at `dst` pointing to `link_target`.
///
/// pre_seed refuses to run on non-Unix platforms (see [`run`]), so symlink creation
/// is Unix-only. On non-Unix the call is unreachable, but we keep a defensive bail.
///
/// SAFETY PRECONDITION (F2): This function only rejects *absolute* symlink targets.
/// It has no `root` parameter and therefore CANNOT validate whether a relative
/// target escapes the extraction root. Callers MUST pre-validate relative targets
/// with [`symlink_escapes`] before calling this. The sole caller, `move_recursive`,
/// receives entries that were already validated during `extract_tarball_with_limits`.
#[allow(unused_variables)]
fn create_symlink(link_target: &Path, dst: &Path) -> anyhow::Result<()> {
    // Reject absolute symlinks unconditionally. Relative-target containment is the
    // caller's responsibility (see SAFETY PRECONDITION above).
    if link_target.is_absolute() {
        anyhow::bail!(
            "seed: rejecting symlink with absolute target: {}",
            link_target.display()
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(link_target, dst)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // Unreachable: run() bails on non-Unix before any extraction happens.
        anyhow::bail!("seed: symlinks are unsupported on non-unix platforms")
    }
}

/// Recursively move files from src directory into dst directory.
/// Checks deadline cooperatively. Unix symlinks are created as-is (target string
/// preserved), so processing order does not matter — no deferred pass needed.
fn move_recursive(src: &Path, dst: &Path, deadline: Instant) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(src)? {
        if Instant::now() >= deadline {
            anyhow::bail!("seed: timed out during move to target");
        }

        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        let meta = src_path.symlink_metadata()?;
        if meta.is_symlink() {
            // Preserve symlinks as-is without following
            let link_target = std::fs::read_link(&src_path)?;
            // Remove existing dst (file or directory) before creating symlink
            if let Ok(dst_meta) = dst_path.symlink_metadata() {
                if dst_meta.is_dir() {
                    std::fs::remove_dir_all(&dst_path)?;
                } else {
                    std::fs::remove_file(&dst_path)?;
                }
            }
            create_symlink(&link_target, &dst_path)?;
        } else if meta.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            move_recursive(&src_path, &dst_path, deadline)?;
        } else {
            if std::fs::rename(&src_path, &dst_path).is_err() {
                std::fs::copy(&src_path, &dst_path)?;
                std::fs::remove_file(&src_path)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    /// Build a gzipped tar in memory from (path, contents) pairs.
    ///
    /// Upstream built these fixtures inline as zips. The properties the tests cover
    /// -- atomic apply, deadline enforcement, byte and entry budgets -- are not
    /// format-specific, so they are preserved against the one format this crate
    /// reads rather than dropped with the zip path.
    fn tgz(files: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let enc = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(enc);
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    use super::*;

    #[test]
    fn extract_and_apply_atomic() {
        let target = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        std::fs::write(target.path().join("existing.txt"), "keep").unwrap();

        let bytes = tgz(&[("new.txt", b"added")]);
        extract_and_apply(&bytes, target.path(), deadline).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.path().join("existing.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            std::fs::read_to_string(target.path().join("new.txt")).unwrap(),
            "added"
        );
    }

    #[test]
    fn extract_respects_expired_deadline() {
        let dir = tempfile::tempdir().unwrap();
        // Already expired deadline
        let deadline = Instant::now() - std::time::Duration::from_secs(1);

        let bytes = tgz(&[("a.txt", b"data")]);
        // extract_and_apply should fail due to expired deadline
        let result = extract_and_apply(&bytes, dir.path(), deadline);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn move_respects_deadline() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("f.txt"), "x").unwrap();

        let expired = Instant::now() - std::time::Duration::from_secs(1);
        let result = move_recursive(src.path(), dst.path(), expired);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn extract_rejects_exceeding_extracted_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        // Three files of ten bytes each: thirty bytes extracted in total.
        let ten = [b'x'; 10];
        let bytes = tgz(&[
            ("file_0.txt", &ten[..]),
            ("file_1.txt", &ten[..]),
            ("file_2.txt", &ten[..]),
        ]);

        // Budget of 20 — fails on the third entry, cumulative 30 > 20.
        let result = extract_tarball_budgeted(&bytes, dir.path(), deadline, 10_000, 20);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("extracted size exceeds limit"),
            "should fail on extracted bytes limit"
        );
    }

    #[test]
    fn extract_rejects_exceeding_file_count() {
        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let bytes = tgz(&[
            ("f_0.txt", b"x"),
            ("f_1.txt", b"x"),
            ("f_2.txt", b"x"),
            ("f_3.txt", b"x"),
            ("f_4.txt", b"x"),
        ]);

        // Budget of 3 entries — should fail, 5 > 3.
        let result = extract_tarball_budgeted(&bytes, dir.path(), deadline, 3, u64::MAX);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("too many entries"),
            "should fail on file count limit"
        );
    }

    #[test]
    fn extract_tarball_basic() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);

        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "hello.txt", &b"world"[..])
            .unwrap();

        let mut header2 = tar::Header::new_gnu();
        header2.set_size(14);
        header2.set_mode(0o644);
        builder
            .append_data(&mut header2, "sub/nested.txt", &b"nested content"[..])
            .unwrap();

        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        extract_tarball_with_limits(&tarball_bytes, dir.path(), deadline).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "world"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/nested.txt")).unwrap(),
            "nested content"
        );
    }

    #[test]
    fn extract_and_apply_detects_tarball_via_magic_bytes() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let target = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "hello.txt", &b"world"[..])
            .unwrap();
        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        // Magic bytes detection — no URI needed
        extract_and_apply(&tarball_bytes, target.path(), deadline).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.path().join("hello.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn extract_tarball_respects_deadline() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = tempfile::tempdir().unwrap();
        let expired = Instant::now() - std::time::Duration::from_secs(1);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);
        // Create > 10 files to trigger deadline check
        for i in 0..11 {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, format!("f{i}.txt"), &b"x"[..])
                .unwrap();
        }
        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        let result = extract_tarball_with_limits(&tarball_bytes, dir.path(), expired);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn move_recursive_preserves_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        // Create a regular file and a symlink pointing to it
        std::fs::write(src.path().join("real.txt"), "content").unwrap();
        std::os::unix::fs::symlink("real.txt", src.path().join("link.txt")).unwrap();

        move_recursive(src.path(), dst.path(), deadline).unwrap();

        let dst_link = dst.path().join("link.txt");
        let meta = dst_link.symlink_metadata().unwrap();
        assert!(meta.is_symlink(), "destination should be a symlink");
        assert_eq!(
            std::fs::read_link(&dst_link).unwrap().to_str().unwrap(),
            "real.txt"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("real.txt")).unwrap(),
            "content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn move_recursive_symlink_overwrites_existing_dir() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        // src has a symlink named "item"
        std::fs::write(src.path().join("target.txt"), "x").unwrap();
        std::os::unix::fs::symlink("target.txt", src.path().join("item")).unwrap();

        // dst has a directory named "item"
        std::fs::create_dir(dst.path().join("item")).unwrap();

        move_recursive(src.path(), dst.path(), deadline).unwrap();

        let dst_item = dst.path().join("item");
        let meta = dst_item.symlink_metadata().unwrap();
        assert!(
            meta.is_symlink(),
            "should have replaced directory with symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_and_apply_succeeds_with_non_writable_parent() {
        use std::os::unix::fs::PermissionsExt;

        // Regression test: target is writable but target.parent() is read-only.
        // Old code used tempdir_in(target.parent()) which would fail here.
        // New code uses tempdir_in(target) which succeeds.
        let base = tempfile::tempdir().unwrap();
        let restricted = base.path().join("restricted");
        std::fs::create_dir(&restricted).unwrap();

        // Create target directory (writable)
        let target = restricted.join("target");
        std::fs::create_dir(&target).unwrap();

        // Lock down parent so tempdir_in(parent) would fail
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o555)).unwrap();

        let bytes = tgz(&[("test.txt", b"data")]);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let result = extract_and_apply(&bytes, &target, deadline);

        // Restore permissions before asserting (for cleanup)
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Should succeed because tempdir_in(target) works even with read-only parent
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("test.txt")).unwrap(),
            "data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_and_apply_succeeds_with_writable_target() {
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("deep").join("target");
        // target doesn't exist yet — create_dir_all should handle it

        let bytes = tgz(&[("test.txt", b"hello")]);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        extract_and_apply(&bytes, &target, deadline).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("test.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn normalize_path_resolves_parent_dir() {
        let p = normalize_path(Path::new("/tmp/extract/bin/../aws-cli/v2"));
        assert_eq!(p, std::path::PathBuf::from("/tmp/extract/aws-cli/v2"));
    }

    #[test]
    fn normalize_path_resolves_multiple_parents() {
        let p = normalize_path(Path::new("/a/b/c/../../d"));
        assert_eq!(p, std::path::PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_path_does_not_go_above_root() {
        let p = normalize_path(Path::new("/a/../../../etc"));
        // Can't go above /, so stays at /etc
        assert_eq!(p, std::path::PathBuf::from("/etc"));
    }

    #[test]
    fn normalize_path_removes_cur_dir() {
        let p = normalize_path(Path::new("/a/./b/./c"));
        assert_eq!(p, std::path::PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_path_drops_leading_parent_dir_on_relative() {
        // F5: leading `..` on a pure-relative path have nothing to pop and are dropped.
        let p = normalize_path(Path::new("../../etc/passwd"));
        assert_eq!(p, std::path::PathBuf::from("etc/passwd"));
    }

    #[test]
    fn symlink_escapes_handles_parent_with_dotdot() {
        // F6: link_path itself contains `..` — normalization resolves it across the
        // whole joined path. dest/a/../b/link -> target ../x resolves to dest/x.
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/a/../b/link");
        let link_target = Path::new("../x");
        // parent = /tmp/extract/a/../b ; join ../x ; normalize -> /tmp/extract/x : within root
        assert!(!symlink_escapes(root, link_path, link_target));
    }

    #[test]
    fn symlink_escapes_parent_with_dotdot_still_blocks_escape() {
        // F6: even with `..` in the entry path, a real escape is still caught.
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/a/../b/link");
        let link_target = Path::new("../../../../etc/passwd");
        assert!(symlink_escapes(root, link_path, link_target));
    }

    #[test]
    fn symlink_escapes_allows_relative_within_root() {
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/bin/aws");
        let link_target = Path::new("../aws-cli/v2/current/bin/aws");
        // ../aws-cli resolves to /tmp/extract/aws-cli — within root
        assert!(!symlink_escapes(root, link_path, link_target));
    }

    #[test]
    fn symlink_escapes_allows_sibling_relative() {
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/aws-cli/v2/current");
        let link_target = Path::new("2.35.12");
        // Resolves to /tmp/extract/aws-cli/v2/2.35.12 — within root
        assert!(!symlink_escapes(root, link_path, link_target));
    }

    #[test]
    fn symlink_escapes_rejects_absolute() {
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/bin/aws");
        let link_target = Path::new("/usr/local/bin/aws");
        assert!(symlink_escapes(root, link_path, link_target));
    }

    #[test]
    fn symlink_escapes_rejects_traversal_above_root() {
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/bin/evil");
        let link_target = Path::new("../../../etc/passwd");
        // Resolves to /tmp/etc/passwd — outside root
        assert!(symlink_escapes(root, link_path, link_target));
    }

    #[test]
    fn symlink_escapes_rejects_deep_traversal() {
        let root = Path::new("/tmp/extract");
        let link_path = Path::new("/tmp/extract/a/b/c");
        let link_target = Path::new("../../../../outside");
        // Resolves to /tmp/outside — outside root
        assert!(symlink_escapes(root, link_path, link_target));
    }

    #[cfg(unix)]
    #[test]
    fn extract_tarball_preserves_relative_symlinks_with_parent_dir() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);

        // Create a real file at aws-cli/bin/aws
        let mut header = tar::Header::new_gnu();
        header.set_size(11);
        header.set_mode(0o755);
        builder
            .append_data(&mut header, "aws-cli/bin/aws", &b"#!/bin/bash"[..])
            .unwrap();

        // Create a relative symlink: bin/aws -> ../aws-cli/bin/aws
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, "bin/aws", "../aws-cli/bin/aws")
            .unwrap();

        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        extract_tarball_with_limits(&tarball_bytes, dir.path(), deadline).unwrap();

        // The symlink should exist and be resolvable
        let symlink_path = dir.path().join("bin/aws");
        assert!(
            symlink_path.symlink_metadata().unwrap().is_symlink(),
            "bin/aws should be a symlink"
        );
        assert_eq!(
            std::fs::read_link(&symlink_path).unwrap(),
            Path::new("../aws-cli/bin/aws")
        );
        // Verify it resolves to the real file
        assert_eq!(
            std::fs::read_to_string(&symlink_path).unwrap(),
            "#!/bin/bash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_tarball_skips_escaping_symlink_without_writing() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);

        // Create an escaping symlink: evil -> ../../../etc/passwd
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, "evil", "../../../etc/passwd")
            .unwrap();

        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        extract_tarball_with_limits(&tarball_bytes, dir.path(), deadline).unwrap();

        // F1: the escaping symlink is skipped before unpacking — it never touches disk
        assert!(
            dir.path().join("evil").symlink_metadata().is_err(),
            "escaping symlink should never be written to disk"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_tarball_allows_valid_hard_link_within_root() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);

        // Create a real file at lib/core.so
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "lib/core.so", &b"ELF\x00\x00\x00\x00"[..])
            .unwrap();

        // F9: Hard link at bin/core.so -> lib/core.so (relative to archive root)
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o644);
        builder
            .append_link(&mut header, "bin/core.so", "lib/core.so")
            .unwrap();

        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        extract_tarball_with_limits(&tarball_bytes, dir.path(), deadline).unwrap();

        // Both files should exist and have identical content
        assert!(dir.path().join("lib/core.so").exists());
        assert!(dir.path().join("bin/core.so").exists());
        assert_eq!(
            std::fs::read(dir.path().join("bin/core.so")).unwrap(),
            std::fs::read(dir.path().join("lib/core.so")).unwrap(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_tarball_skips_escaping_hard_link() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);

        // F9: Hard link with escaping target -> ../../../etc/passwd (relative to archive root)
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o644);
        builder
            .append_link(&mut header, "evil_link", "../../../etc/passwd")
            .unwrap();

        let enc = builder.into_inner().unwrap();
        let tarball_bytes = enc.finish().unwrap();

        extract_tarball_with_limits(&tarball_bytes, dir.path(), deadline).unwrap();

        // Escaping hard link should be skipped — never written to disk
        assert!(
            dir.path().join("evil_link").symlink_metadata().is_err(),
            "escaping hard link should never be written to disk"
        );
    }
}
