//! Build nx-aarch64 toolchain release packages.
//!
//! For each target, concurrently:
//!   1. Download the official LLVM release tarball into toolchains/downloads/,
//!      verified against the sha256 published on GitHub (skipped if already
//!      downloaded with a matching hash)
//!   2. Extract only the files in list-of-files.txt to toolchains/<target>/
//!      (multithreaded xz decoding)
//!   3. Copy __config_site into include/c++/v1
//!   4. Package the extracted files (bin/ and include/ at the top level) into
//!      dist/nx-aarch64-<llvm-version>-<revision>-<target>.tar.xz
//!      (multithreaded xz encoding)

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::ValueEnum as _;
use cu::pre::*;
use liblzma::stream::{Check, Filters, LzmaOptions, MtStreamBuilder, PRESET_EXTREME};
use sha2::{Digest, Sha256};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// xz settings for the output packages. The dictionary and block size are
/// small so compression can be split across many threads without using too
/// much memory (~200MiB per thread), at the cost of ~5% larger output.
const XZ_PRESET: u32 = 9 | PRESET_EXTREME;
const XZ_BLOCK_SIZE: u32 = 16 << 20;
const XZ_MAX_THREADS: u32 = 16;
/// Memory budget for each multithreaded decoder, which limits its threads
const XZ_DECODER_MEMLIMIT: u64 = 512 << 20;

/// Compression is the most memory hungry step, so only package one target at
/// a time (it is already multithreaded)
static PACKAGE_LOCK: Mutex<()> = Mutex::new(());

type Bar = Arc<cu::ProgressBar>;

#[derive(clap::Parser, AsRef)]
struct Cli {
    /// LLVM version to package
    #[clap(long, default_value = "22.1.8")]
    llvm_version: String,

    /// Only build these targets (can be repeated; default: all)
    #[clap(short, long, value_enum)]
    target: Vec<Target>,

    #[clap(flatten)]
    #[as_ref]
    common: cu::cli::Flags,
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
enum Target {
    LinuxX64,
    LinuxArm64,
    MacosArm64,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Target::LinuxX64 => "linux-x64",
            Target::LinuxArm64 => "linux-arm64",
            Target::MacosArm64 => "macos-arm64",
        }
    }

    /// The <os>-<arch> part of LLVM's release asset name
    fn llvm_name(self) -> &'static str {
        match self {
            Target::LinuxX64 => "Linux-X64",
            Target::LinuxArm64 => "Linux-ARM64",
            Target::MacosArm64 => "macOS-ARM64",
        }
    }
}

#[cu::cli]
async fn main(args: Cli) -> cu::Result<()> {
    let root = Path::new(ROOT);
    let version = args.llvm_version;
    let revision = read_revision(&cu::path!(&root / "revision.txt"))?;
    let keep = Arc::new(read_file_list(&cu::path!(&root / "list-of-files.txt"))?);

    let mut targets = args.target;
    if targets.is_empty() {
        targets = Target::value_variants().to_vec();
    }
    targets.dedup();

    let digests = {
        let version = version.clone();
        cu::co::spawn_blocking(move || fetch_llvm_digests(&version))
            .co_join()
            .await??
    };

    let mut jobs = Vec::new();
    let mut skipped = Vec::new();
    for target in targets {
        let asset = format!("LLVM-{version}-{}.tar.xz", target.llvm_name());
        let Some(sha256) = digests.get(&asset) else {
            cu::warn!(
                "{asset} is not published for LLVM {version}, skipping {}",
                target.name()
            );
            skipped.push(target);
            continue;
        };
        let Some(sha256) = sha256.clone() else {
            cu::bail!("GitHub has no sha256 digest for {asset}, cannot verify it");
        };
        let url = format!(
            "https://github.com/llvm/llvm-project/releases/download/llvmorg-{version}/{asset}"
        );
        let out_name = format!("nx-aarch64-{version}-{revision}-{}.tar.xz", target.name());
        jobs.push(Job {
            target,
            archive: cu::path!(&root / "toolchains" / "downloads" / asset),
            asset,
            url,
            sha256,
            dir: cu::path!(&root / "toolchains" / (target.name())),
            config_site: cu::path!(&root / "__config_site"),
            keep: Arc::clone(&keep),
            out: cu::path!(&root / "dist" / out_name),
        });
    }
    cu::ensure!(!jobs.is_empty(), "nothing to build")?;

    let bar = cu::progress(format!("nx-aarch64 {version}-{revision}"))
        .total(jobs.len())
        .eta(false)
        .percentage(false)
        .spawn();
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|job| {
            let bar = Arc::clone(&bar);
            let name = job.target.name();
            (name, cu::co::spawn_blocking(move || job.run(&bar)))
        })
        .collect();

    let mut built = Vec::new();
    for (name, handle) in handles {
        built.push(cu::check!(
            handle.co_join().await?,
            "failed to build {name}"
        )?);
    }
    drop(bar);

    for out in built {
        cu::info!("built: {}", out.display());
    }
    for target in skipped {
        cu::warn!(
            "skipped: {} (no LLVM {version} release asset)",
            target.name()
        );
    }
    Ok(())
}

fn read_revision(path: &Path) -> cu::Result<String> {
    let revision = cu::fs::read_string(path)?;
    let revision = revision.trim();
    cu::ensure!(
        !revision.is_empty() && revision.bytes().all(|b| b.is_ascii_digit()),
        "revision.txt must contain a number, got {revision:?}"
    )?;
    Ok(revision.to_string())
}

fn read_file_list(path: &Path) -> cu::Result<Vec<String>> {
    let list = cu::fs::read_string(path)?;
    Ok(list
        .lines()
        .map(|line| line.trim().trim_matches('/'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect())
}

/// Return {asset name: sha256 hex} for the LLVM release, from the GitHub API
fn fetch_llvm_digests(version: &str) -> cu::Result<BTreeMap<String, Option<String>>> {
    let url =
        format!("https://api.github.com/repos/llvm/llvm-project/releases/tags/llvmorg-{version}");
    let mut request = ureq::get(&url).header("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = cu::check!(
        request.call(),
        "failed to fetch LLVM {version} release info"
    )?;
    let body = cu::check!(response.body_mut().read_to_string(), "failed to read {url}")?;
    let release: cu::json::Value = cu::json::parse(&body)?;

    let mut digests = BTreeMap::new();
    for asset in release["assets"].as_array().into_iter().flatten() {
        let Some(name) = asset["name"].as_str() else {
            continue;
        };
        let digest = asset["digest"]
            .as_str()
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(String::from);
        digests.insert(name.to_string(), digest);
    }
    Ok(digests)
}

struct Job {
    target: Target,
    asset: String,
    url: String,
    sha256: String,
    archive: PathBuf,
    dir: PathBuf,
    config_site: PathBuf,
    keep: Arc<Vec<String>>,
    out: PathBuf,
}

impl Job {
    fn run(self, parent: &Bar) -> cu::Result<PathBuf> {
        let name = self.target.name();
        let bar = parent.child(name).keep(true).spawn();

        cu::progress!(bar, "download");
        self.download(&bar)?;

        cu::progress!(bar, "extract");
        self.extract(&bar)?;
        let libcxx_dir = cu::path!(&(self.dir) / "include" / "c++" / "v1");
        cu::ensure!(
            libcxx_dir.is_dir(),
            "{} does not exist",
            libcxx_dir.display()
        )?;
        cu::fs::copy(&self.config_site, cu::path!(libcxx_dir / "__config_site"))?;

        cu::progress!(bar, "waiting to package");
        let lock = PACKAGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        cu::progress!(bar, "package");
        package(&bar, &self.dir, &self.out)?;
        drop(lock);

        let file_name = self.out.file_name().unwrap_or_default().to_string_lossy();
        bar.done_with_message(&format!("{name}: {file_name}"));
        cu::progress!(parent += 1);
        Ok(self.out)
    }

    /// Download the archive, verifying its sha256. Skips if it's already
    /// downloaded with a matching hash
    fn download(&self, bar: &Bar) -> cu::Result<()> {
        let name = self.target.name();
        if self.archive.exists() {
            let hash = sha256_file(bar, &self.archive)?;
            if hash == self.sha256 {
                cu::info!("{name}: {} already downloaded, sha256 ok", self.asset);
                return Ok(());
            }
            cu::warn!("{name}: {} has wrong sha256, downloading again", self.asset);
            cu::fs::remove(&self.archive)?;
        }
        if let Some(parent) = self.archive.parent() {
            cu::fs::make_dir(parent)?;
        }
        let part = self.archive.with_added_extension("part");
        let result = self.download_to(bar, &part);
        if result.is_err() {
            let _ = cu::fs::remove(&part);
        }
        result?;
        cu::fs::rename(&part, &self.archive)?;
        cu::info!("{name}: downloaded {}, sha256 ok", self.asset);
        Ok(())
    }

    fn download_to(&self, bar: &Bar, path: &Path) -> cu::Result<()> {
        let mut response = cu::check!(
            ureq::get(&self.url).call(),
            "failed to download {}",
            self.url
        )?;
        let total = response.body().content_length().unwrap_or(0);
        let child = bar
            .child(format!("download {}", self.asset))
            .total_bytes(total)
            .spawn();
        let mut reader = ProgressReader::new(response.body_mut().as_reader(), Arc::clone(&child));
        let mut out = cu::fs::buf_writer(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; 1 << 20];
        loop {
            let n = cu::check!(reader.read(&mut buf), "failed to download {}", self.url)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        child.set_total(reader.read);
        let hash = hex::encode(hasher.finalize());
        cu::ensure!(
            hash == self.sha256,
            "sha256 mismatch for {}\n  expected {}\n  got      {hash}",
            self.asset,
            self.sha256
        )?;
        Ok(())
    }

    /// Extract only the paths in keep (and their contents), stripping the
    /// archive's top-level directory. The archive is streamed once.
    fn extract(&self, bar: &Bar) -> cu::Result<()> {
        cu::fs::make_dir_empty(&self.dir)?;

        let size = file_size(&self.archive)?;
        let child = bar
            .child(format!("extract {}", self.asset))
            .total_bytes(size)
            .spawn();
        let reader = ProgressReader::new(cu::fs::reader(&self.archive)?, Arc::clone(&child));
        let stream = MtStreamBuilder::new()
            .threads(num_threads())
            .memlimit_threading(XZ_DECODER_MEMLIMIT)
            .memlimit_stop(u64::MAX)
            .decoder()?;
        let mut archive = tar::Archive::new(liblzma::read::XzDecoder::new_stream(reader, stream));

        let mut found = BTreeSet::new();
        for entry in cu::check!(archive.entries(), "failed to read {}", self.asset)? {
            let mut entry = cu::check!(entry, "failed to read {}", self.asset)?;
            let rel = strip_top(&entry.path()?);
            let Some(k) = self.keep.iter().find(|k| rel.starts_with(k.as_str())) else {
                continue;
            };
            found.insert(k.as_str());

            let out = cu::path!(&(self.dir) / rel);
            if let Some(parent) = out.parent() {
                cu::fs::make_dir(parent)?;
            }
            if entry.header().entry_type().is_hard_link() {
                let link = entry
                    .link_name()?
                    .map(|l| strip_top(&l))
                    .unwrap_or_default();
                let link = cu::path!(&(self.dir) / link);
                cu::check!(
                    fs::hard_link(&link, &out),
                    "failed to link {}",
                    rel.display()
                )?;
            } else {
                cu::check!(entry.unpack(&out), "failed to extract {}", rel.display())?;
            }
        }
        cu::progress!(child = size);

        let missing: Vec<_> = self
            .keep
            .iter()
            .filter(|k| !found.contains(k.as_str()))
            .collect();
        for k in &missing {
            cu::error!("{}: {k} not found in {}", self.target.name(), self.asset);
        }
        cu::ensure!(
            missing.is_empty(),
            "{} path(s) from list-of-files.txt missing",
            missing.len()
        )?;
        Ok(())
    }
}

/// Package the contents of src_dir (at the top level of the tarball) to out
fn package(bar: &Bar, src_dir: &Path, out: &Path) -> cu::Result<()> {
    let mut entries = Vec::new();
    collect_entries(src_dir, Path::new(""), &mut entries)?;
    // estimated tar size, for the progress bar
    let total: u64 = entries
        .iter()
        .map(|(_, size)| 512 + size.div_ceil(512) * 512)
        .sum();

    if let Some(parent) = out.parent() {
        cu::fs::make_dir(parent)?;
    }
    let child = bar
        .child(format!("package {}", out.display()))
        .total_bytes(total)
        .spawn();
    let part = out.with_added_extension("part");
    let result = write_package(&child, src_dir, &entries, &part);
    if result.is_err() {
        let _ = cu::fs::remove(&part);
    }
    result?;
    cu::progress!(child = total);
    cu::fs::rename(&part, out)?;
    Ok(())
}

fn write_package(
    bar: &Bar,
    src_dir: &Path,
    entries: &[(PathBuf, u64)],
    out: &Path,
) -> cu::Result<()> {
    let mut opts = LzmaOptions::new_preset(XZ_PRESET)?;
    opts.dict_size(XZ_BLOCK_SIZE);
    let mut filters = Filters::new();
    filters.lzma2(&opts);
    let stream = MtStreamBuilder::new()
        .threads(num_threads().min(XZ_MAX_THREADS))
        .block_size(XZ_BLOCK_SIZE as u64)
        .filters(filters)
        .check(Check::Crc64)
        .encoder()?;

    let xz = liblzma::write::XzEncoder::new_stream(cu::fs::buf_writer(out)?, stream);
    let mut tar = tar::Builder::new(ProgressWriter {
        inner: xz,
        bar: Arc::clone(bar),
    });
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    for (rel, _) in entries {
        let path = cu::path!(&src_dir / rel);
        cu::check!(
            tar.append_path_with_name(path, rel),
            "failed to package {}",
            rel.display()
        )?;
    }
    let xz = tar.into_inner()?.inner;
    xz.finish()?.flush()?;
    Ok(())
}

/// Recursively list (relative path, file size) under dir, sorted, without
/// following symlinks
fn collect_entries(root: &Path, rel: &Path, out: &mut Vec<(PathBuf, u64)>) -> cu::Result<()> {
    let mut names = Vec::new();
    for entry in cu::fs::read_dir(cu::path!(&root / rel))? {
        names.push(entry?.file_name());
    }
    names.sort();
    for name in names {
        let child = cu::path!(&rel / name);
        let path = cu::path!(&root / child);
        let meta = cu::check!(
            fs::symlink_metadata(&path),
            "failed to read metadata of {}",
            path.display()
        )?;
        if meta.is_dir() {
            out.push((child.clone(), 0));
            collect_entries(root, &child, out)?;
        } else if meta.is_file() {
            out.push((child, meta.len()));
        } else {
            out.push((child, 0));
        }
    }
    Ok(())
}

fn sha256_file(bar: &Bar, path: &Path) -> cu::Result<String> {
    let size = file_size(path)?;
    let child = bar
        .child(format!("verify {}", path.display()))
        .total_bytes(size)
        .spawn();
    let mut reader = ProgressReader::new(cu::fs::reader(path)?, child);
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = cu::check!(reader.read(&mut buf), "failed to read {}", path.display())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn file_size(path: &Path) -> cu::Result<u64> {
    let meta = cu::check!(
        fs::metadata(path),
        "failed to read metadata of {}",
        path.display()
    )?;
    Ok(meta.len())
}

/// Remove the first component of an archive path
fn strip_top(path: &Path) -> PathBuf {
    path.components().skip(1).collect()
}

fn num_threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// Reader that updates a progress bar with the number of bytes read
struct ProgressReader<R> {
    inner: R,
    bar: Bar,
    read: u64,
}

impl<R> ProgressReader<R> {
    fn new(inner: R, bar: Bar) -> Self {
        Self {
            inner,
            bar,
            read: 0,
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        let bar = &self.bar;
        cu::progress!(bar += n);
        Ok(n)
    }
}

/// Writer that updates a progress bar with the number of bytes written
struct ProgressWriter<W> {
    inner: W,
    bar: Bar,
}

impl<W: Write> Write for ProgressWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        let bar = &self.bar;
        cu::progress!(bar += n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
