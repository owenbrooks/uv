use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use itertools::Itertools;
use owo_colors::OwoColorize;
use rustc_hash::FxHashSet;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;
use tracing::debug;

use uv_client::Connectivity;
use uv_python::downloads::{DownloadResult, ManagedPythonDownload, PythonDownloadRequest};
use uv_python::managed::{ManagedPythonInstallation, ManagedPythonInstallations};
use uv_python::{PythonDownloads, PythonRequest, PythonVersionFile};

use crate::commands::python::{ChangeEvent, ChangeEventKind};
use crate::commands::reporters::PythonDownloadReporter;
use crate::commands::{elapsed, ExitStatus};
use crate::printer::Printer;

/// Download and install Python versions.
pub(crate) async fn install(
    project_dir: &Path,
    targets: Vec<String>,
    reinstall: bool,
    python_downloads: PythonDownloads,
    native_tls: bool,
    connectivity: Connectivity,
    no_config: bool,
    printer: Printer,
) -> Result<ExitStatus> {
    let start = std::time::Instant::now();

    let installations = ManagedPythonInstallations::from_settings()?.init()?;
    let installations_dir = installations.root();
    let cache_dir = installations.cache();
    let _lock = installations.lock().await?;

    let targets = targets.into_iter().collect::<BTreeSet<_>>();
    let requests: Vec<_> = if targets.is_empty() {
        PythonVersionFile::discover(project_dir, no_config, true)
            .await?
            .map(PythonVersionFile::into_versions)
            .unwrap_or_else(|| vec![PythonRequest::Default])
    } else {
        targets
            .iter()
            .map(|target| PythonRequest::parse(target.as_str()))
            .collect()
    };

    let download_requests = requests
        .iter()
        .map(|request| {
            PythonDownloadRequest::from_request(request).ok_or_else(|| {
                anyhow::anyhow!("Cannot download managed Python for request: {request}")
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let installed_installations: Vec<_> = installations
        .find_all()?
        .inspect(|installation| debug!("Found existing installation {}", installation.key()))
        .collect();
    let mut unfilled_requests = Vec::new();
    let mut uninstalled = FxHashSet::default();
    for (request, download_request) in requests.iter().zip(download_requests) {
        if matches!(requests.as_slice(), [PythonRequest::Default]) {
            writeln!(printer.stderr(), "Searching for Python installations")?;
        } else {
            writeln!(
                printer.stderr(),
                "Searching for Python versions matching: {}",
                request.cyan()
            )?;
        }
        if let Some(installation) = installed_installations
            .iter()
            .find(|installation| download_request.satisfied_by_key(installation.key()))
        {
            if matches!(request, PythonRequest::Default) {
                writeln!(printer.stderr(), "Found: {}", installation.key().green())?;
            } else {
                writeln!(
                    printer.stderr(),
                    "Found existing installation for {}: {}",
                    request.cyan(),
                    installation.key().green(),
                )?;
            }
            if reinstall {
                uninstalled.insert(installation.key());
                unfilled_requests.push(download_request);
            }
        } else {
            unfilled_requests.push(download_request);
        }
    }

    if unfilled_requests.is_empty() {
        if matches!(requests.as_slice(), [PythonRequest::Default]) {
            writeln!(
                printer.stderr(),
                "Python is already available. Use `uv python install <request>` to install a specific version.",
            )?;
        } else if requests.len() > 1 {
            writeln!(printer.stderr(), "All requested versions already installed")?;
        }
        return Ok(ExitStatus::Success);
    }

    if matches!(python_downloads, PythonDownloads::Never) {
        writeln!(
            printer.stderr(),
            "Python downloads are not allowed (`python-downloads = \"never\"`). Change to `python-downloads = \"manual\"` to allow explicit installs.",
        )?;
        return Ok(ExitStatus::Failure);
    }

    let downloads = unfilled_requests
        .into_iter()
        // Populate the download requests with defaults
        .map(|request| ManagedPythonDownload::from_request(&PythonDownloadRequest::fill(request)?))
        .collect::<Result<Vec<_>, uv_python::downloads::Error>>()?;

    // Ensure we only download each version once
    let downloads = downloads
        .into_iter()
        .unique_by(|download| download.key())
        .collect::<Vec<_>>();

    // Construct a client
    let client = uv_client::BaseClientBuilder::new()
        .connectivity(connectivity)
        .native_tls(native_tls)
        .build();

    let reporter = PythonDownloadReporter::new(printer, downloads.len() as u64);

    let mut tasks = FuturesUnordered::new();
    for download in &downloads {
        tasks.push(async {
            (
                download.key(),
                download
                    .fetch(
                        &client,
                        installations_dir,
                        &cache_dir,
                        reinstall,
                        Some(&reporter),
                    )
                    .await,
            )
        });
    }

    let mut installed = FxHashSet::default();
    let mut errors = vec![];
    while let Some((key, result)) = tasks.next().await {
        match result {
            Ok(download) => {
                let path = match download {
                    // We should only encounter already-available during concurrent installs
                    DownloadResult::AlreadyAvailable(path) => path,
                    DownloadResult::Fetched(path) => path,
                };

                installed.insert(key);

                // Ensure the installations have externally managed markers
                let managed = ManagedPythonInstallation::new(path.clone())?;
                managed.ensure_externally_managed()?;
                managed.ensure_canonical_executables()?;
            }
            Err(err) => {
                errors.push((key, err));
            }
        }
    }

    if !installed.is_empty() {
        if installed.len() == 1 {
            let installed = installed.iter().next().unwrap();
            // Ex) "Installed Python 3.9.7 in 1.68s"
            writeln!(
                printer.stderr(),
                "{}",
                format!(
                    "Installed {} {}",
                    format!("Python {}", installed.version()).bold(),
                    format!("in {}", elapsed(start.elapsed())).dimmed()
                )
                .dimmed()
            )?;
        } else {
            // Ex) "Installed 2 versions in 1.68s"
            writeln!(
                printer.stderr(),
                "{}",
                format!(
                    "Installed {} {}",
                    format!("{} versions", installed.len()).bold(),
                    format!("in {}", elapsed(start.elapsed())).dimmed()
                )
                .dimmed()
            )?;
        }

        let reinstalled = uninstalled
            .intersection(&installed)
            .copied()
            .collect::<FxHashSet<_>>();
        let uninstalled = uninstalled.difference(&reinstalled).copied();
        let installed = installed.difference(&reinstalled).copied();

        for event in uninstalled
            .map(|key| ChangeEvent {
                key: key.clone(),
                kind: ChangeEventKind::Removed,
            })
            .chain(installed.map(|key| ChangeEvent {
                key: key.clone(),
                kind: ChangeEventKind::Added,
            }))
            .chain(reinstalled.iter().map(|&key| ChangeEvent {
                key: key.clone(),
                kind: ChangeEventKind::Reinstalled,
            }))
            .sorted_unstable_by(|a, b| a.key.cmp(&b.key).then_with(|| a.kind.cmp(&b.kind)))
        {
            match event.kind {
                ChangeEventKind::Added => {
                    writeln!(printer.stderr(), " {} {}", "+".green(), event.key.bold())?;
                }
                ChangeEventKind::Removed => {
                    writeln!(printer.stderr(), " {} {}", "-".red(), event.key.bold())?;
                }
                ChangeEventKind::Reinstalled => {
                    writeln!(printer.stderr(), " {} {}", "~".yellow(), event.key.bold(),)?;
                }
            }
        }
    }

    if !errors.is_empty() {
        for (key, err) in errors
            .into_iter()
            .sorted_unstable_by(|(key_a, _), (key_b, _)| key_a.cmp(key_b))
        {
            writeln!(
                printer.stderr(),
                "{}: Failed to install {}",
                "error".red().bold(),
                key.green()
            )?;
            for err in anyhow::Error::new(err).chain() {
                writeln!(
                    printer.stderr(),
                    "  {}: {}",
                    "Caused by".red().bold(),
                    err.to_string().trim()
                )?;
            }
        }
        return Ok(ExitStatus::Failure);
    }

    Ok(ExitStatus::Success)
}
