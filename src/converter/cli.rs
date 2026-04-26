use super::model::{OutputFormat, DEFAULT_INPUT_DIR, DEFAULT_OUTPUT_DIR};
use super::{note, render};
use crate::Result;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub(crate) fn run(args: Vec<String>) -> Result<()> {
    let options = parse_args(&args)?;
    let input = options
        .input
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INPUT_DIR));
    if !input.exists() {
        fs::create_dir_all(&input)?;
        println!("Created input; add .note files there, then run again.");
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
    prepare_output_dir(input, output)?;
    let files = source_files(input)?;
    validate_collisions(input, output, &files, format)?;

    for entry in WalkDir::new(input)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_dir() {
            let relative = entry.path().strip_prefix(input)?;
            fs::create_dir_all(output.join(relative))?;
        }
    }

    let mut count = 0usize;
    for source in files {
        let target = output_path_for_source(input, output, &source, format)?;
        if source
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("note"))
        {
            let document = note::load_note_document(&source)?;
            render::write_note_output(&document, &source, &target, format)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &target)?;
        }
        count += 1;
    }
    println!("Mirrored {count} file(s) to {}", output.display());
    Ok(())
}

fn prepare_output_dir(input: &Path, output: &Path) -> Result<()> {
    let input = input.canonicalize()?;
    if output.exists() {
        let output_canon = output.canonicalize()?;
        if input == output_canon {
            return Err("Output folder must be different from input folder".into());
        }
        if input.starts_with(&output_canon) {
            return Err("Output folder must not contain the input folder".into());
        }
        if !output.is_dir() {
            return Err(format!(
                "Output path exists and is not a folder: {}",
                output.display()
            )
            .into());
        }
        fs::remove_dir_all(output)?;
    }
    fs::create_dir_all(output)?;
    Ok(())
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
    if source
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("note"))
    {
        Ok(output.join(relative).with_extension(format.extension()))
    } else {
        Ok(output.join(relative))
    }
}
