use super::model::{OutputFormat, DEFAULT_INPUT_DIR, DEFAULT_OUTPUT_DIR};
use super::{note, ntb, render};
use crate::Result;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

#[cfg(test)]
mod tests;

pub(crate) fn run(args: Vec<String>) -> Result<()> {
    let options = parse_args(&args)?;
    let input = options
        .input
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INPUT_DIR));
    if !input.exists() {
        fs::create_dir_all(&input)?;
        println!("Created input; add .note or .ntb files there, then run again.");
        return Ok(());
    }
    if !input.is_dir() {
        return Err(format!("Input path is not a folder: {}", input.display()).into());
    }
    let output_dir = options
        .output
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIR));
    mirror_folder(&input, &output_dir, options.format)?;
    Ok(())
}

struct CliOptions {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    format: OutputFormat,
}

fn parse_args(args: &[String]) -> Result<CliOptions> {
    let mut options = CliOptions {
        input: None,
        output: None,
        format: OutputFormat::Pdf,
    };
    let mut index = 0usize;
    while index < args.len() {
        let flag = &args[index];
        let path = args.get(index + 1).ok_or_else(|| usage_error())?;
        match flag.as_str() {
            "--input" | "-i" => options.input = Some(PathBuf::from(path)),
            "--output" | "-o" => options.output = Some(PathBuf::from(path)),
            "--format" | "-f" => options.format = parse_format(path)?,
            _ => return Err(usage_error()),
        }
        index += 2;
    }
    Ok(options)
}

fn parse_format(value: &str) -> Result<OutputFormat> {
    match value {
        "pdf" => Ok(OutputFormat::Pdf),
        "svg" => Ok(OutputFormat::Svg),
        _ => Err(format!("Unsupported format: {value}. Use pdf or svg.").into()),
    }
}

fn usage_error() -> Box<dyn std::error::Error> {
    "usage: cargo run -- [--input <path>] [--output <path>] [--format <pdf|svg>]".into()
}

fn mirror_folder(input: &Path, output: &Path, format: OutputFormat) -> Result<()> {
    let input = input.canonicalize()?;
    let output = validate_output_dir(&input, output)?;
    let backups = validate_output_dir(&input, &backup_root(&output)?)?;
    if backups.starts_with(&output) || output.starts_with(&backups) {
        return Err("Backup folder must be separate from the output folder".into());
    }
    let files = source_files(&input)?;
    validate_collisions(&input, &output, &files, format)?;
    let mut staging = OwnedDirectory::beside(&output, "staging")?;
    let count = mirror_into(&input, &staging.path, files, format)?;
    let backup = publish_output(&mut staging, &output, &backups)?;
    println!("Mirrored {count} file(s) to {}", output.display());
    if let Some(backup) = backup {
        println!("Previous output kept at {}", backup.display());
    }
    Ok(())
}

fn mirror_into(
    input: &Path,
    output: &Path,
    files: Vec<PathBuf>,
    format: OutputFormat,
) -> Result<usize> {
    for entry in WalkDir::new(input) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            let relative = entry.path().strip_prefix(input)?;
            fs::create_dir_all(output.join(relative))?;
        }
    }

    let mut count = 0usize;
    let total = files.len();
    println!("Found {total} file(s).");
    for source in files {
        let index = count + 1;
        let target = output_path_for_source(input, output, &source, format)?;
        let relative = source.strip_prefix(input)?;
        if is_note_source(&source) {
            print!("[{index}/{total}] Converting {} ... ", relative.display());
            io::stdout().flush()?;
            let document = if has_extension(&source, "ntb") {
                ntb::load_ntb_document(&source)?
            } else {
                note::load_note_document(&source)?
            };
            render::write_note_output(&document, &source, &target, format)?;
            println!("done");
        } else {
            print!("[{index}/{total}] Copying {} ... ", relative.display());
            io::stdout().flush()?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &target)?;
            println!("done");
        }
        count += 1;
    }
    Ok(count)
}

fn validate_output_dir(input: &Path, output: &Path) -> Result<PathBuf> {
    let output = resolve_destination(output)?;
    if input == output {
        return Err("Output folder must be different from input folder".into());
    }
    if input.starts_with(&output) {
        return Err("Output folder must not contain the input folder".into());
    }
    if output.starts_with(input) {
        return Err("Output folder must not be inside the input folder".into());
    }
    if output.exists() && !output.is_dir() {
        return Err(format!(
            "Output path exists and is not a folder: {}",
            output.display()
        )
        .into());
    }
    Ok(output)
}

// Resolve existing symlinks as we walk, including before handling `..`.
// The final directory need not exist yet.
fn resolve_destination(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        resolved = resolved.canonicalize()?;
                    }
                    Ok(metadata) if metadata.is_dir() => {
                        resolved = resolved.canonicalize()?;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(resolved)
}

struct OwnedDirectory {
    path: PathBuf,
    cleanup: bool,
}

fn backup_root(output: &Path) -> Result<PathBuf> {
    let mut name = output
        .file_name()
        .ok_or("Output folder has no name")?
        .to_os_string();
    name.push(".backups");
    Ok(output.with_file_name(name))
}

impl OwnedDirectory {
    fn beside(output: &Path, label: &str) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = output.parent().ok_or("Output folder has no parent")?;
        fs::create_dir_all(parent)?;
        let name = output
            .file_name()
            .ok_or("Output folder has no name")?
            .to_string_lossy()
            .chars()
            .take(32)
            .collect::<String>();
        loop {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".{name}.{label}-{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        cleanup: true,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn backup_in(root: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(root)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        loop {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "backup-{timestamp}-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        cleanup: true,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        if self.cleanup {
            // Only directories successfully created by this instance are removed.
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn publish_output(
    staging: &mut OwnedDirectory,
    output: &Path,
    backups: &Path,
) -> Result<Option<PathBuf>> {
    let backup = if output.exists() {
        let mut directory = OwnedDirectory::backup_in(backups)?;
        // Reserve a unique name, then move the entire output into that snapshot.
        // Removing this newly created empty directory supports Windows renames.
        fs::remove_dir(&directory.path)?;
        directory.cleanup = false;
        fs::rename(output, &directory.path)?;
        Some(directory.path.clone())
    } else {
        None
    };
    if let Err(error) = fs::rename(&staging.path, output) {
        if let Some(previous) = &backup {
            if let Err(rollback) = fs::rename(previous, output) {
                return Err(format!("Could not publish output: {error}. Could not restore previous output: {rollback}. Previous output is at {}", previous.display()).into());
            }
        }
        return Err(error.into());
    }
    staging.cleanup = false;
    Ok(backup)
}

fn source_files(input: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(input) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn validate_collisions(
    input: &Path,
    output: &Path,
    files: &[PathBuf],
    format: OutputFormat,
) -> Result<()> {
    let mut targets = BTreeMap::new();
    for source in files {
        let target = output_path_for_source(input, output, source, format)?;
        if let Some(previous) = targets.insert(target.clone(), source.clone()) {
            return Err(format!(
                "Output path collision: {} and {} both map to {}",
                previous.strip_prefix(input)?.display(),
                source.strip_prefix(input)?.display(),
                target.strip_prefix(output)?.display()
            )
            .into());
        }
    }
    Ok(())
}

fn output_path_for_source(
    input: &Path,
    output: &Path,
    source: &Path,
    format: OutputFormat,
) -> Result<PathBuf> {
    let relative = source.strip_prefix(input)?;
    if is_note_source(source) {
        Ok(output.join(relative).with_extension(format.extension()))
    } else {
        Ok(output.join(relative))
    }
}

fn is_note_source(path: &Path) -> bool {
    has_extension(path, "note") || has_extension(path, "ntb")
}

fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}
