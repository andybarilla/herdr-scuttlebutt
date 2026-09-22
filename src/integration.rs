use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Pi,
    Opencode,
}

impl Target {
    fn all() -> &'static [Target] {
        &[Target::Pi, Target::Opencode]
    }

    fn name(self) -> &'static str {
        match self {
            Target::Pi => "pi",
            Target::Opencode => "opencode",
        }
    }
}

#[derive(Debug)]
struct LinkSpec {
    target: Target,
    source: PathBuf,
    dest: PathBuf,
}

pub fn parse_targets(name: &str) -> Result<Vec<Target>> {
    match name {
        "all" => Ok(Target::all().to_vec()),
        "pi" => Ok(vec![Target::Pi]),
        "opencode" => Ok(vec![Target::Opencode]),
        other => bail!("unknown integration target {other:?} (expected all, pi, or opencode)"),
    }
}

pub fn install(targets: &[Target], force: bool) -> Result<()> {
    for spec in specs(targets)? {
        install_one(&spec, force)?;
        println!(
            "{}: installed {} -> {}",
            spec.target.name(),
            spec.dest.display(),
            spec.source.display()
        );
    }
    Ok(())
}

pub fn status(targets: &[Target]) -> Result<()> {
    for spec in specs(targets)? {
        println!("{}: {}", spec.target.name(), status_one(&spec));
    }
    Ok(())
}

pub fn uninstall(targets: &[Target]) -> Result<()> {
    for spec in specs(targets)? {
        match fs::symlink_metadata(&spec.dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&spec.dest)
                    .with_context(|| format!("reading {}", spec.dest.display()))?;
                if link == spec.source {
                    fs::remove_file(&spec.dest)
                        .with_context(|| format!("removing {}", spec.dest.display()))?;
                    println!("{}: removed {}", spec.target.name(), spec.dest.display());
                } else {
                    println!(
                        "{}: left {} alone (points to {})",
                        spec.target.name(),
                        spec.dest.display(),
                        link.display()
                    );
                }
            }
            Ok(_) => println!(
                "{}: left {} alone (not a symlink)",
                spec.target.name(),
                spec.dest.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("{}: not installed", spec.target.name())
            }
            Err(e) => return Err(e).with_context(|| format!("checking {}", spec.dest.display())),
        }
    }
    Ok(())
}

fn specs(targets: &[Target]) -> Result<Vec<LinkSpec>> {
    let root = repo_root()?;
    let home = home_dir()?;
    let mut out = Vec::new();
    for target in targets {
        match target {
            Target::Pi => out.push(LinkSpec {
                target: *target,
                source: root.join(".pi/extensions/scuttlebutt.ts"),
                dest: home.join(".pi/agent/extensions/scuttlebutt.ts"),
            }),
            Target::Opencode => {
                out.push(LinkSpec {
                    target: *target,
                    source: root.join(".opencode/plugins/scuttlebutt/index.ts"),
                    dest: home.join(".config/opencode/plugins/scuttlebutt/index.ts"),
                });
                out.push(LinkSpec {
                    target: *target,
                    source: root.join(".opencode/tools/scuttlebutt.ts"),
                    dest: home.join(".config/opencode/tools/scuttlebutt.ts"),
                });
            }
        }
    }
    for spec in &out {
        if !spec.source.exists() {
            bail!("integration source is missing: {}", spec.source.display());
        }
    }
    Ok(out)
}

fn install_one(spec: &LinkSpec, force: bool) -> Result<()> {
    if let Some(parent) = spec.dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    match fs::symlink_metadata(&spec.dest) {
        Ok(meta) => {
            let replace = force
                || (meta.file_type().is_symlink()
                    && fs::read_link(&spec.dest)
                        .map(|link| link == spec.source)
                        .unwrap_or(false));
            if !replace {
                bail!(
                    "{} already exists; remove it or pass --force: {}",
                    spec.dest.display(),
                    spec.target.name()
                );
            }
            fs::remove_file(&spec.dest)
                .with_context(|| format!("removing {}", spec.dest.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("checking {}", spec.dest.display())),
    }
    symlink_file(&spec.source, &spec.dest)
        .with_context(|| format!("linking {}", spec.dest.display()))?;
    Ok(())
}

fn status_one(spec: &LinkSpec) -> String {
    match fs::symlink_metadata(&spec.dest) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(&spec.dest) {
            Ok(link) if link == spec.source => format!("current ({})", spec.dest.display()),
            Ok(link) => format!(
                "different symlink: {} -> {}",
                spec.dest.display(),
                link.display()
            ),
            Err(e) => format!("unreadable symlink {} ({e})", spec.dest.display()),
        },
        Ok(_) => format!("blocked by non-symlink {}", spec.dest.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("not installed ({})", spec.dest.display())
        }
        Err(e) => format!("unknown ({}: {e})", spec.dest.display()),
    }
}

fn repo_root() -> Result<PathBuf> {
    if let Ok(root) = std::env::var("SCUTTLEBUTT_REPO_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let exe = std::env::current_exe().context("locating scuttlebutt executable")?;
    let root = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("scuttlebutt executable is not under <repo>/target/<profile>")?;
    Ok(root.to_path_buf())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

#[cfg(unix)]
fn symlink_file(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, dest)
}

#[cfg(windows)]
fn symlink_file(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(source, dest)
}
