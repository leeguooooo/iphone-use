//! The on-phone daemon library (`libipu_embed.a`, `crates/ios-embed`) that
//! setup links into the device runner, so the phone serves the agent API
//! itself (see `runner/README.md`, "The on-phone daemon").
//!
//! Where it comes from, first match wins:
//! 1. `IPU_EMBED=0`: nowhere, the runner is built without it.
//! 2. `IPU_EMBED_LIB`: an absolute path to the library.
//! 3. `<runner sources>/embed/libipu_embed.a`, which the installer lays down
//!    from the release.
//! 4. A repo checkout (`<repo>/runner`): `scripts/build-ios-embed.sh` builds
//!    it into `<repo>/target`. The KeepAlive supervisor never runs cargo; it
//!    links what the last interactive setup built.
//!
//! A missing library is not an error: the plain runner is built and serves
//! as before. The library's content hash is part of the product cache key, so
//! a new library rebuilds the runner and an unchanged one reuses it.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The library's file name, wherever it is.
pub const LIB_NAME: &str = "libipu_embed.a";
/// Under the runner sources: where the installer puts the release's library.
pub const SHIPPED_DIR: &str = "embed";

/// A library to link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedLib {
    pub path: PathBuf,
    /// sha256 of the file's content, lowercase hex.
    pub sha256: String,
}

/// What [`resolve`] reads.
pub struct Inputs<'a> {
    /// `IPU_EMBED` as configured.
    pub embed: &'a str,
    /// `IPU_EMBED_LIB` as configured.
    pub embed_lib: &'a str,
    pub runner_src: &'a Path,
    /// The KeepAlive supervisor's run: never builds.
    pub keepalive: bool,
}

/// The repo a checkout's `runner/` belongs to, when it is one that can build
/// the library.
pub fn checkout_repo(runner_src: &Path) -> Option<PathBuf> {
    if runner_src.file_name()? != "runner" {
        return None;
    }
    let repo = runner_src.parent()?;
    (repo.join("scripts/build-ios-embed.sh").is_file()
        && repo.join("crates/ios-embed/Cargo.toml").is_file())
    .then(|| repo.to_path_buf())
}

/// Where `scripts/build-ios-embed.sh` leaves the library in `repo`.
pub fn checkout_lib(repo: &Path) -> PathBuf {
    repo.join("target/aarch64-apple-ios/release").join(LIB_NAME)
}

/// The library to link and one line saying where it came from (or why there
/// is none). `build` builds it in a checkout and says whether that worked.
pub fn resolve(
    inputs: &Inputs<'_>,
    build: impl FnOnce(&Path) -> bool,
) -> (Option<EmbedLib>, String) {
    if inputs.embed.trim() == "0" {
        return (
            None,
            "On-phone daemon off (IPU_EMBED=0); building the runner without it".into(),
        );
    }
    let explicit = inputs.embed_lib.trim();
    if !explicit.is_empty() {
        let path = PathBuf::from(explicit);
        return match usable(&path) {
            Ok(lib) => (
                Some(lib),
                format!("On-phone daemon library: {explicit} (IPU_EMBED_LIB)"),
            ),
            Err(why) => (
                None,
                format!("IPU_EMBED_LIB {why}; building the runner without the on-phone daemon"),
            ),
        };
    }
    let shipped = inputs.runner_src.join(SHIPPED_DIR).join(LIB_NAME);
    if std::fs::symlink_metadata(&shipped).is_ok() {
        return match usable(&shipped) {
            Ok(lib) => (
                Some(lib),
                format!("On-phone daemon library: {}", shipped.display()),
            ),
            Err(why) => (
                None,
                format!(
                    "the shipped on-phone daemon library {why}; building the runner without it"
                ),
            ),
        };
    }
    let Some(repo) = checkout_repo(inputs.runner_src) else {
        return (
            None,
            format!(
                "No on-phone daemon library ({} is missing); building the runner without it",
                shipped.display()
            ),
        );
    };
    let built = checkout_lib(&repo);
    let fresh = !inputs.keepalive && build(&repo);
    match usable(&built) {
        Ok(lib) if fresh => (
            Some(lib),
            format!("On-phone daemon library built: {}", built.display()),
        ),
        // Better the last build than a product that flips between the
        // supervisor's relaunches and the next interactive setup.
        Ok(lib) => (
            Some(lib),
            format!(
                "On-phone daemon library: {} (last build{})",
                built.display(),
                if inputs.keepalive {
                    ""
                } else {
                    "; this build failed"
                }
            ),
        ),
        Err(_) => (
            None,
            if inputs.keepalive {
                "No on-phone daemon library built in this checkout yet; building the runner without it".into()
            } else {
                "Could not build the on-phone daemon library (scripts/build-ios-embed.sh); building the runner without it".into()
            },
        ),
    }
}

/// `path` as a library to link, or why it cannot be.
fn usable(path: &Path) -> Result<EmbedLib, String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("must be an absolute path (got {shown})"));
    }
    // xcodebuild splits OTHER_LDFLAGS on whitespace and reads quotes.
    if path
        .to_string_lossy()
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\' | '$'))
    {
        return Err(format!(
            "path has a space or quote xcodebuild cannot pass on ({shown})"
        ));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|_| format!("does not exist ({shown})"))?;
    if !meta.file_type().is_file() {
        return Err(format!("is not a plain file ({shown})"));
    }
    let sha256 = file_sha256(path).ok_or_else(|| format!("could not be read ({shown})"))?;
    Ok(EmbedLib {
        path: path.to_path_buf(),
        sha256,
    })
}

fn file_sha256(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).ok()?;
    Some(
        digest
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// The xcodebuild setting that links `lib` (as `runner/build.sh` does).
/// `-force_load`: nothing references the entry points at link time (the
/// runner finds them with dlsym), so a plain link would drop them.
pub fn xcodebuild_setting(lib: &EmbedLib) -> String {
    format!(
        "OTHER_LDFLAGS=$(inherited) -force_load {} -framework Security -framework SystemConfiguration",
        lib.path.display()
    )
}

/// The cache-key component: the library's content, not its path or mtime.
pub fn cache_component(lib: Option<&EmbedLib>) -> String {
    match lib {
        Some(lib) => format!("embed:{}", lib.sha256),
        None => "embed:none".into(),
    }
}

/// `env` argv that runs the checkout's build script with its target dir
/// pinned (so the library lands at [`checkout_lib`]) and rustup's cargo on
/// PATH.
pub fn build_argv(repo: &Path, home: &Path) -> Vec<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    vec![
        format!("IPU_EMBED_TARGET_DIR={}", repo.join("target").display()),
        format!("PATH={}:{path}", home.join(".cargo/bin").display()),
        "/bin/bash".into(),
        repo.join("scripts/build-ios-embed.sh")
            .to_string_lossy()
            .into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(embed: &'a str, lib: &'a str, src: &'a Path, keepalive: bool) -> Inputs<'a> {
        Inputs {
            embed,
            embed_lib: lib,
            runner_src: src,
            keepalive,
        }
    }

    fn checkout(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(root.join("crates/ios-embed")).unwrap();
        std::fs::create_dir_all(root.join("runner")).unwrap();
        std::fs::write(root.join("scripts/build-ios-embed.sh"), "").unwrap();
        std::fs::write(root.join("crates/ios-embed/Cargo.toml"), "").unwrap();
        root.join("runner")
    }

    fn never(_: &Path) -> bool {
        panic!("must not build")
    }

    #[test]
    fn opt_out_and_missing_library_build_the_plain_runner() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("runner");
        std::fs::create_dir_all(src.join(SHIPPED_DIR)).unwrap();
        std::fs::write(src.join(SHIPPED_DIR).join(LIB_NAME), b"lib").unwrap();
        let (lib, line) = resolve(&inputs("0", "", &src, false), never);
        assert!(lib.is_none());
        assert!(line.contains("IPU_EMBED=0"));

        let bare = dir.path().join("installed/runner");
        std::fs::create_dir_all(&bare).unwrap();
        let (lib, line) = resolve(&inputs("", "", &bare, false), never);
        assert!(lib.is_none());
        assert!(line.contains("without it"), "{line}");
    }

    #[test]
    fn explicit_then_shipped_library() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("runner");
        std::fs::create_dir_all(src.join(SHIPPED_DIR)).unwrap();
        let shipped = src.join(SHIPPED_DIR).join(LIB_NAME);
        std::fs::write(&shipped, b"shipped").unwrap();
        let explicit = dir.path().join("mine.a");
        std::fs::write(&explicit, b"mine").unwrap();
        let explicit_text = explicit.to_string_lossy().into_owned();

        let (lib, _) = resolve(&inputs("", &explicit_text, &src, false), never);
        assert_eq!(lib.unwrap().path, explicit);
        let (lib, _) = resolve(&inputs("1", "", &src, false), never);
        assert_eq!(lib.unwrap().path, shipped);
        // A relative or missing IPU_EMBED_LIB is reported, not fatal.
        let (lib, line) = resolve(&inputs("", "lib.a", &src, false), never);
        assert!(lib.is_none() && line.contains("absolute"), "{line}");
        let spaced = dir.path().join("my lib.a");
        std::fs::write(&spaced, b"x").unwrap();
        let (lib, _) = resolve(&inputs("", &spaced.to_string_lossy(), &src, false), never);
        assert!(lib.is_none());
    }

    #[test]
    fn a_checkout_builds_it_but_the_supervisor_never_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = checkout(&root);
        assert_eq!(checkout_repo(&src).as_deref(), Some(root.as_path()));
        let built = checkout_lib(&root);

        // Interactive: the build runs and its product is linked.
        let (lib, _) = resolve(&inputs("", "", &src, false), |repo| {
            std::fs::create_dir_all(checkout_lib(repo).parent().unwrap()).unwrap();
            std::fs::write(checkout_lib(repo), b"v1").unwrap();
            true
        });
        assert_eq!(lib.as_ref().unwrap().path, built);
        // Supervisor: no build, the last one is linked.
        let (again, line) = resolve(&inputs("", "", &src, true), never);
        assert_eq!(again, lib, "{line}");
        // A failed build falls back to the last one.
        let (fallback, line) = resolve(&inputs("", "", &src, false), |_| false);
        assert_eq!(fallback, lib);
        assert!(line.contains("failed"), "{line}");
    }

    #[test]
    fn the_cache_component_follows_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIB_NAME);
        std::fs::write(&path, b"one").unwrap();
        let first = usable(&path).unwrap();
        let same = usable(&path).unwrap();
        std::fs::write(&path, b"two").unwrap();
        let changed = usable(&path).unwrap();
        assert_eq!(cache_component(Some(&first)), cache_component(Some(&same)));
        assert_ne!(
            cache_component(Some(&first)),
            cache_component(Some(&changed))
        );
        assert_eq!(
            cache_component(Some(&first)),
            format!("embed:{:x}", Sha256::digest(b"one"))
        );
        assert_eq!(cache_component(None), "embed:none");
        assert_eq!(
            xcodebuild_setting(&first),
            format!(
                "OTHER_LDFLAGS=$(inherited) -force_load {} -framework Security -framework SystemConfiguration",
                path.display()
            )
        );
    }
}
