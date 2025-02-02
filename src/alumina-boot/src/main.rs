#![allow(clippy::single_match)]
#![allow(clippy::mutable_key_type)]

mod ast;
mod codegen;
mod common;
mod compiler;
mod diagnostics;
mod global_ctx;
mod intrinsics;
mod ir;
mod name_resolution;
mod parser;
mod utils;
mod visitors;

use crate::common::{AluminaError, CodeError};
use crate::compiler::{Compiler, SourceFile};
use crate::global_ctx::{GlobalCtx, OutputType};

use clap::builder::ValueParser;
use clap::Parser;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use walkdir::WalkDir;

fn parse_module(s: &str) -> Result<(Option<String>, PathBuf), std::convert::Infallible> {
    Ok(if let Some(pos) = s.find('=') {
        (Some(s[..pos].to_string()), s[pos + 1..].to_string().into())
    } else {
        (None, s.to_string().into())
    })
}

fn parse_cfg(s: &str) -> Result<(String, Option<String>), std::convert::Infallible> {
    Ok(if let Some(pos) = s.find('=') {
        (s[..pos].to_string(), Some(s[pos + 1..].to_string()))
    } else {
        (s.to_string(), None)
    })
}

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    /// Output filename (defaults to stdout)
    #[clap(short, long)]
    output: Option<String>,

    /// Path to the standard library
    #[clap(long, env = "ALUMINA_SYSROOT")]
    sysroot: Option<PathBuf>,

    /// Modules to compile ('module::name=filename.alu')
    #[clap(value_parser=ValueParser::new(parse_module))]
    modules: Vec<(Option<String>, PathBuf)>,

    /// Compile in debug mode
    #[clap(long, short)]
    debug: bool,

    /// Collect timings
    #[clap(long)]
    timings: bool,

    /// Whether a library should be output
    #[clap(long)]
    library: bool,

    /// Conditional compilation options
    #[clap(long, value_parser=ValueParser::new(parse_cfg), action=clap::ArgAction::Append)]
    cfg: Vec<(String, Option<String>)>,

    /// Unstable compiler options
    #[clap(long, short('Z'), action=clap::ArgAction::Append)]
    options: Vec<String>,
}

fn infer_module_name(path: &std::path::Path) -> &str {
    path.file_stem().unwrap().to_str().unwrap()
}

fn get_sysroot(args: &Args) -> Result<Vec<SourceFile>, AluminaError> {
    let mut result = Vec::new();

    let sysroot = if let Some(sysroot) = &args.sysroot {
        sysroot
    } else {
        return Ok(vec![]);
    };

    for maybe_entry in WalkDir::new(sysroot).follow_links(true).into_iter() {
        use std::fmt::Write;
        let entry = maybe_entry?;
        if entry.file_type().is_dir() {
            continue;
        }

        let filename = entry.file_name().to_string_lossy();
        if !filename.ends_with(".alu") {
            continue;
        }

        let path_segments: Vec<_> = entry
            .path()
            .strip_prefix(sysroot)
            .unwrap()
            .iter()
            .map(|s| s.to_string_lossy())
            .collect();

        let mut module_path = String::new();
        for (index, segment) in path_segments.iter().enumerate() {
            if index < path_segments.len() - 1 {
                write!(module_path, "::{}", segment).unwrap();
            } else {
                let module_name = segment.strip_suffix(".alu").unwrap();
                if module_name != "__root__" {
                    write!(module_path, "::{}", module_name).unwrap();
                }
            }
        }
        if module_path.is_empty() {
            module_path.push_str("::");
        }
        result.push(SourceFile {
            filename: entry.into_path(),
            path: module_path,
        });
    }

    Ok(result)
}

fn run(args: Args) -> Result<(), ()> {
    let start_time = Instant::now();
    let output_type = if args.library {
        OutputType::Library
    } else {
        OutputType::Executable
    };

    let mut global_ctx = GlobalCtx::new(output_type, args.options.clone());
    let mut compiler = Compiler::new(global_ctx.clone());

    let mut files = get_sysroot(&args).unwrap();
    for (path, filename) in &args.modules {
        files.push(SourceFile {
            filename: filename.clone(),
            path: path
                .as_deref()
                .unwrap_or_else(|| infer_module_name(filename))
                .to_string(),
        });
    }

    for (key, value) in args.cfg {
        if let Some(value) = value {
            global_ctx.add_cfg(key, value)
        } else {
            global_ctx.add_flag(key)
        }
    }

    if args.debug {
        global_ctx.add_flag("debug");
    }

    match compiler.compile(files, start_time) {
        Ok(program) => {
            let diag_ctx = global_ctx.diag();
            if args.timings {
                for (stage, duration) in compiler.timings() {
                    diag_ctx.add_note(CodeError::freeform(format!(
                        "compiler timings: stage {:?} took {}ms",
                        stage,
                        duration.as_millis()
                    )));
                }
            }
            diag_ctx.print_error_report().unwrap();
            if diag_ctx.has_errors() {
                return Err(());
            }
            match args.output {
                Some(filename) => std::fs::write(filename, program).unwrap(),
                None => {
                    print!("{}", program);
                }
            }
        }
        Err(e) => {
            let diag_ctx = global_ctx.diag();
            diag_ctx.add_from_error(e).unwrap();
            diag_ctx.print_error_report().unwrap();
            return Err(());
        }
    }

    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    if run(args).is_err() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
