use anyhow::Context;
use anyhow::Result;
#[cfg(not(windows))]
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use codex_instruction_control::ForkConfig;
use codex_instruction_control::PolicySnapshot;
use codex_instruction_control::PolicySource;
use codex_instruction_control::SourceKind;
use std::path::Path;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Review and explicitly accept protected Codex instruction snapshots")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prepare a candidate without changing accepted instructions or requesting elevation.
    Prepare {
        #[arg(long)]
        main: PathBuf,
        #[arg(long)]
        catalog: PathBuf,
        /// JSON ForkConfig; omitted means the documented defaults and empty attribution.
        #[arg(long)]
        controls: Option<PathBuf>,
        /// Additional immutable source: KIND=LOGICAL_ID=FILE.
        #[arg(long = "source")]
        sources: Vec<String>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Add or replace sources in a candidate. This does not accept the result.
    Extend {
        #[arg(long)]
        candidate: PathBuf,
        #[arg(long = "source", required = true)]
        sources: Vec<String>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Print the complete accepted snapshot, or compare a candidate with it.
    Inspect {
        #[arg(long)]
        candidate: Option<PathBuf>,
    },
    /// Verify installed bytes, accepted sources, and denied ordinary-client write access.
    Verify,
    /// Apply a candidate through the installed, protected elevated helper and native review UI.
    Apply {
        #[arg(long)]
        candidate: PathBuf,
    },
    /// First installation. Copies the distribution into Program Files after UAC and review.
    Install {
        #[arg(long)]
        distribution: PathBuf,
        #[arg(long)]
        candidate: PathBuf,
    },
    /// Replace installed binaries after UAC and review, preserving accepted instructions.
    Upgrade {
        #[arg(long)]
        distribution: PathBuf,
    },
}

fn read_snapshot(path: &Path) -> Result<PolicySnapshot> {
    PolicySnapshot::parse(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
}

fn source(spec: &str) -> Result<PolicySource> {
    let mut fields = spec.splitn(3, '=');
    let kind = fields.next().context("source kind missing")?;
    let id = fields.next().context("source logical ID missing")?;
    let path = fields
        .next()
        .context("source path missing; expected KIND=LOGICAL_ID=FILE")?;
    let kind: SourceKind = serde_json::from_value(serde_json::Value::String(kind.into()))?;
    if kind == SourceKind::ModelCatalog {
        Ok(PolicySource::new(
            kind,
            id,
            codex_instruction_control::normalize_model_catalog(&std::fs::read_to_string(path)?)?,
        ))
    } else if matches!(
        kind,
        SourceKind::Asset
            | SourceKind::SkillAsset
            | SourceKind::PluginAsset
            | SourceKind::HookAsset
    ) {
        Ok(PolicySource::asset(kind, id, std::fs::read(path)?))
    } else {
        Ok(PolicySource::new(kind, id, std::fs::read_to_string(path)?))
    }
}

fn save(path: &Path, snapshot: &PolicySnapshot) -> Result<()> {
    // Candidate paths are user-selected; never write through an existing file or link.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&serde_json::to_vec_pretty(snapshot)?)?;
    file.sync_all()?;
    println!("Candidate {}: {}", snapshot.snapshot_id, path.display());
    Ok(())
}

fn main() -> Result<()> {
    let command = Cli::parse().command;
    #[cfg(windows)]
    if matches!(&command, Command::Prepare { .. } | Command::Extend { .. })
        && codex_instruction_control::windows::is_elevated()?
    {
        anyhow::bail!(
            "prepare candidates without elevation; only reviewed install/apply operations may write with administrator rights"
        );
    }
    match command {
        Command::Prepare {
            main,
            catalog,
            controls,
            sources,
            output,
        } => {
            let config = controls
                .map(|p| -> Result<ForkConfig> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) })
                .transpose()?
                .unwrap_or_default();
            let mut entries = vec![
                PolicySource::new(
                    SourceKind::MainPrompt,
                    "main_instructions",
                    std::fs::read_to_string(main)?,
                ),
                PolicySource::new(
                    SourceKind::ModelCatalog,
                    "model_catalog",
                    codex_instruction_control::normalize_model_catalog(&std::fs::read_to_string(
                        catalog,
                    )?)?,
                ),
            ];
            for spec in sources {
                entries.push(source(&spec)?);
            }
            save(&output, &PolicySnapshot::new(config, entries)?)
        }
        Command::Extend {
            candidate,
            sources,
            output,
        } => {
            let snapshot = read_snapshot(&candidate)?;
            let mut entries = snapshot.sources;
            for spec in sources {
                let entry = source(&spec)?;
                entries.retain(|existing| {
                    existing.kind != entry.kind || existing.logical_id != entry.logical_id
                });
                entries.push(entry);
            }
            save(&output, &PolicySnapshot::new(snapshot.config, entries)?)
        }
        Command::Inspect { candidate } => {
            let accepted = PolicySnapshot::load_protected();
            if let Some(path) = candidate {
                let candidate = read_snapshot(&path)?;
                if let Err(error) = &accepted {
                    eprintln!("Accepted snapshot unavailable: {error:#}");
                }
                println!(
                    "{}",
                    codex_instruction_control::review_text(accepted.as_ref().ok(), &candidate)?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&accepted?)?);
            }
            Ok(())
        }
        Command::Verify => {
            #[cfg(windows)]
            {
                codex_instruction_control::windows::verify_client_permissions()
            }
            #[cfg(not(windows))]
            {
                bail!("protected installation verification requires Windows")
            }
        }
        Command::Apply { candidate } => {
            #[cfg(windows)]
            {
                codex_instruction_control::windows::apply(&candidate)
            }
            #[cfg(not(windows))]
            {
                let _ = candidate;
                bail!("protected instruction updates require Windows")
            }
        }
        Command::Install {
            distribution,
            candidate,
        } => {
            #[cfg(windows)]
            {
                codex_instruction_control::windows::install(&distribution, &candidate)
            }
            #[cfg(not(windows))]
            {
                let _ = (distribution, candidate);
                bail!("protected installation requires Windows")
            }
        }
        Command::Upgrade { distribution } => {
            #[cfg(windows)]
            {
                codex_instruction_control::windows::upgrade(&distribution)
            }
            #[cfg(not(windows))]
            {
                let _ = distribution;
                bail!("protected binary upgrades require Windows")
            }
        }
    }
}
