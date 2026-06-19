// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line stack budget checker for stackful binaries.
//!
//! `kimojio-stack-check` reads compiler-emitted `.stack_sizes` metadata and a
//! disassembled call graph, then reports the largest known stack usage reachable
//! from one or more entry-point patterns. It is intended for projects using
//! small guarded stackful coroutines where stack budgets should be validated
//! before lowering default stack sizes.
//!
//! The tool is conservative: direct call edges are followed, cycles and indirect
//! calls are reported as unknowns, and `--fail-on-unknown` can turn those
//! unknowns into a failing gate. It does not prove stack usage for recursion,
//! function pointers, trait-object dispatch, dynamically loaded code, or code
//! without stack-size metadata.
//!
//! # Typical workflow
//!
//! 1. Build the target with stack-size metadata, for example with
//!    `RUSTFLAGS="-Z emit-stack-sizes"` on nightly.
//! 2. Preserve the `.stack_sizes` section in the final binary.
//! 3. Run this checker against entry functions that correspond to stackful task
//!    roots.
//!
//! ```sh
//! cargo run -p kimojio-stack-check -- \
//!   --binary target/release/object-gateway-stack-host \
//!   --entry serve_stack_admin_listener=20480 \
//!   --entry serve_steal_grpc_listener=20480 \
//!   --fail-on-unknown
//! ```
//!
//! # Output
//!
//! Each matching entry prints `frame_bytes`, `known_stack_bytes`, optional
//! `budget_bytes`, and counts of unknown edges/cycles. Over-budget entries make
//! the process exit with an error.
//!
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rustc_demangle::demangle;

#[derive(Debug)]
struct Args {
    binary: PathBuf,
    entries: Vec<EntrySpec>,
    default_budget: Option<usize>,
    readobj: OsString,
    objdump: OsString,
    fail_on_unknown: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntrySpec {
    pattern: String,
    budget: Option<usize>,
}

#[derive(Debug)]
struct Program {
    frames: HashMap<String, usize>,
    graph: HashMap<String, FunctionCalls>,
}

#[derive(Debug, Default)]
struct FunctionCalls {
    direct: Vec<String>,
    indirect_sites: usize,
}

#[derive(Debug)]
struct Analysis<'a> {
    program: &'a Program,
    memo: HashMap<String, usize>,
    unknowns: Vec<UnknownEdge>,
    cycles: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
struct UnknownEdge {
    caller: String,
    reason: String,
}

fn main() -> Result<()> {
    let args = parse_args(env::args_os().skip(1))?;
    let stack_sizes = read_stack_sizes(&args.readobj, &args.binary)?;
    ensure!(
        !stack_sizes.is_empty(),
        "no stack-size metadata found in {}; build with -Z emit-stack-sizes and preserve .stack_sizes with kimojio-stack-check/keep-stack-sizes.x",
        args.binary.display()
    );
    let graph = read_call_graph(&args.objdump, &args.binary)?;
    let program = Program {
        frames: stack_sizes,
        graph,
    };
    let mut failed = false;
    for entry in &args.entries {
        let matches = matching_symbols(&program, &entry.pattern);
        ensure!(
            !matches.is_empty(),
            "entry pattern {:?} did not match any function symbol",
            entry.pattern
        );
        for symbol in matches {
            let mut analysis = Analysis::new(&program);
            let known_bytes = analysis.known_stack_bytes(&symbol);
            let budget = entry.budget.or(args.default_budget);
            let demangled = display_symbol(&symbol);
            let frame = program.frames.get(&symbol).copied().unwrap_or_default();
            print!(
                "stack-check entry_pattern={:?} symbol={:?} frame_bytes={} known_stack_bytes={}",
                entry.pattern, demangled, frame, known_bytes
            );
            if let Some(budget) = budget {
                print!(" budget_bytes={budget}");
                if known_bytes > budget {
                    failed = true;
                    print!(" status=over-budget");
                } else {
                    print!(" status=within-known-budget");
                }
            } else {
                print!(" status=measured");
            }
            println!(
                " unknown_edges={} cycles={}",
                analysis.unknowns.len(),
                analysis.cycles.len()
            );
            for unknown in analysis.unknowns.iter().take(20) {
                println!(
                    "stack-check-unknown caller={:?} reason={:?}",
                    display_symbol(&unknown.caller),
                    unknown.reason
                );
            }
            if analysis.unknowns.len() > 20 {
                println!(
                    "stack-check-unknown-truncated omitted={}",
                    analysis.unknowns.len() - 20
                );
            }
            for cycle in analysis.cycles.iter().take(10) {
                let path = cycle
                    .iter()
                    .map(|symbol| display_symbol(symbol))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                println!("stack-check-cycle path={path:?}");
            }
            if analysis.cycles.len() > 10 {
                println!(
                    "stack-check-cycle-truncated omitted={}",
                    analysis.cycles.len() - 10
                );
            }
            if args.fail_on_unknown
                && (!analysis.unknowns.is_empty() || !analysis.cycles.is_empty())
            {
                failed = true;
            }
        }
    }
    if failed {
        bail!("one or more stack entries failed configured checks");
    }
    Ok(())
}

impl Analysis<'_> {
    fn new(program: &Program) -> Analysis<'_> {
        Analysis {
            program,
            memo: HashMap::new(),
            unknowns: Vec::new(),
            cycles: Vec::new(),
        }
    }

    fn known_stack_bytes(&mut self, symbol: &str) -> usize {
        let mut visiting = Vec::new();
        self.known_stack_bytes_inner(symbol, &mut visiting)
    }

    fn known_stack_bytes_inner(&mut self, symbol: &str, visiting: &mut Vec<String>) -> usize {
        if let Some(bytes) = self.memo.get(symbol) {
            return *bytes;
        }
        if let Some(position) = visiting.iter().position(|visited| visited == symbol) {
            self.cycles.push(visiting[position..].to_vec());
            return *self.program.frames.get(symbol).unwrap_or(&0);
        }
        visiting.push(symbol.to_owned());
        let frame = *self.program.frames.get(symbol).unwrap_or(&0);
        let mut max_child = 0usize;
        let calls = self.program.graph.get(symbol);
        if let Some(calls) = calls {
            if calls.indirect_sites > 0 {
                self.unknowns.push(UnknownEdge {
                    caller: symbol.to_owned(),
                    reason: format!("{} indirect call site(s)", calls.indirect_sites),
                });
            }
            for callee in &calls.direct {
                if self.program.frames.contains_key(callee) {
                    max_child = max_child.max(self.known_stack_bytes_inner(callee, visiting));
                } else if is_external_symbol(callee) {
                    self.unknowns.push(UnknownEdge {
                        caller: symbol.to_owned(),
                        reason: format!("external callee {}", display_symbol(callee)),
                    });
                }
            }
        }
        let _ = visiting.pop();
        let total = frame.saturating_add(max_child);
        self.memo.insert(symbol.to_owned(), total);
        total
    }
}

fn parse_args<I>(mut args: I) -> Result<Args>
where
    I: Iterator<Item = OsString>,
{
    let mut binary = None;
    let mut entries = Vec::new();
    let mut default_budget = None;
    let mut readobj = OsString::from("llvm-readobj");
    let mut objdump = OsString::from("llvm-objdump");
    let mut fail_on_unknown = false;
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| anyhow!("arguments must be valid UTF-8"))?;
        match arg.as_str() {
            "--binary" => binary = Some(next_path(&mut args, "--binary")?),
            "--entry" => entries.push(parse_entry(&next_string(&mut args, "--entry")?)?),
            "--budget" => default_budget = Some(parse_bytes(&next_string(&mut args, "--budget")?)?),
            "--readobj" => readobj = OsString::from(next_string(&mut args, "--readobj")?),
            "--objdump" => objdump = OsString::from(next_string(&mut args, "--objdump")?),
            "--fail-on-unknown" => fail_on_unknown = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            _ => bail!("unknown argument {arg:?}; use --help"),
        }
    }
    let binary = binary.context("--binary is required")?;
    ensure!(!entries.is_empty(), "at least one --entry is required");
    Ok(Args {
        binary,
        entries,
        default_budget,
        readobj,
        objdump,
        fail_on_unknown,
    })
}

fn next_string<I>(args: &mut I, flag: &str) -> Result<String>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .with_context(|| format!("{flag} requires a value"))?
        .into_string()
        .map_err(|_| anyhow!("{flag} value must be valid UTF-8"))
}

fn next_path<I>(args: &mut I, flag: &str) -> Result<PathBuf>
where
    I: Iterator<Item = OsString>,
{
    Ok(PathBuf::from(next_string(args, flag)?))
}

fn parse_entry(value: &str) -> Result<EntrySpec> {
    let Some((pattern, budget)) = value.rsplit_once('=') else {
        return Ok(EntrySpec {
            pattern: value.to_owned(),
            budget: None,
        });
    };
    ensure!(!pattern.is_empty(), "entry pattern must not be empty");
    Ok(EntrySpec {
        pattern: pattern.to_owned(),
        budget: Some(parse_bytes(budget)?),
    })
}

fn parse_bytes(value: &str) -> Result<usize> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1024usize),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1024usize * 1024),
        _ => (value, 1usize),
    };
    let bytes = digits
        .parse::<usize>()
        .with_context(|| format!("invalid byte count {value:?}"))?;
    bytes
        .checked_mul(multiplier)
        .with_context(|| format!("byte count {value:?} overflows usize"))
}

fn print_usage() {
    println!(
        "Usage: kimojio-stack-check --binary <elf> --entry <symbol-substring>[=<budget>] [--budget <bytes>] [--fail-on-unknown]\n\
         \n\
         Budgets accept raw bytes or K/M suffixes. The binary must be built with rustc -Z emit-stack-sizes and linked with keep-stack-sizes.x."
    );
}

fn read_stack_sizes(readobj: &OsString, binary: &Path) -> Result<HashMap<String, usize>> {
    let output = Command::new(readobj)
        .arg("--elf-output-style=GNU")
        .arg("--stack-sizes")
        .arg(binary)
        .output()
        .with_context(|| format!("failed to run {}", readobj.to_string_lossy()))?;
    ensure!(
        output.status.success(),
        "llvm-readobj failed with status {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_stack_sizes(&String::from_utf8_lossy(&output.stdout))
}

fn parse_stack_sizes(text: &str) -> Result<HashMap<String, usize>> {
    let mut frames = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(size) = fields.next() else {
            continue;
        };
        let Ok(size) = size.parse::<usize>() else {
            continue;
        };
        let Some(symbol) = fields.next() else {
            continue;
        };
        frames.insert(normalize_symbol(symbol), size);
    }
    Ok(frames)
}

fn read_call_graph(objdump: &OsString, binary: &Path) -> Result<HashMap<String, FunctionCalls>> {
    let output = Command::new(objdump)
        .arg("-d")
        .arg("--no-show-raw-insn")
        .arg(binary)
        .output()
        .with_context(|| format!("failed to run {}", objdump.to_string_lossy()))?;
    ensure!(
        output.status.success(),
        "llvm-objdump failed with status {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(parse_call_graph(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_call_graph(text: &str) -> HashMap<String, FunctionCalls> {
    let mut graph = HashMap::<String, FunctionCalls>::new();
    let mut current = None::<String>;
    for line in text.lines() {
        if let Some(symbol) = parse_function_label(line) {
            current = Some(symbol.clone());
            graph.entry(symbol).or_default();
            continue;
        }
        if !line.contains("call") {
            continue;
        }
        let Some(caller) = current.as_ref() else {
            continue;
        };
        let calls = graph.entry(caller.clone()).or_default();
        if line.contains("callq\t*") || line.contains("call\t*") || !line.contains('<') {
            calls.indirect_sites += 1;
            continue;
        }
        if let Some(target) = parse_bracket_symbol(line) {
            calls.direct.push(target);
        } else {
            calls.indirect_sites += 1;
        }
    }
    for calls in graph.values_mut() {
        calls.direct.sort();
        calls.direct.dedup();
    }
    graph
}

fn parse_function_label(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.ends_with(':') {
        return None;
    }
    let open = trimmed.find('<')?;
    let close = trimmed[open + 1..].find('>')? + open + 1;
    Some(normalize_symbol(&trimmed[open + 1..close]))
}

fn parse_bracket_symbol(line: &str) -> Option<String> {
    let open = line.rfind('<')?;
    let close = line[open + 1..].find('>')? + open + 1;
    Some(normalize_symbol(&line[open + 1..close]))
}

fn normalize_symbol(symbol: &str) -> String {
    let without_offset = symbol.split('+').next().unwrap_or(symbol);
    without_offset
        .strip_suffix("@plt")
        .unwrap_or(without_offset)
        .to_owned()
}

fn matching_symbols(program: &Program, pattern: &str) -> Vec<String> {
    let mut symbols = program
        .frames
        .keys()
        .chain(program.graph.keys())
        .filter(|symbol| symbol.contains(pattern) || display_symbol(symbol).contains(pattern))
        .cloned()
        .collect::<Vec<_>>();
    symbols.sort();
    symbols.dedup();
    symbols
}

fn display_symbol(symbol: &str) -> String {
    demangle(symbol).to_string()
}

fn is_external_symbol(symbol: &str) -> bool {
    !(symbol.starts_with("_R")
        || symbol.starts_with("_ZN")
        || symbol.starts_with("_$LT$")
        || symbol.starts_with("main")
        || symbol.starts_with("__rust"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stack_sizes() {
        let parsed = parse_stack_sizes(
            "\
Stack Sizes:\n\
         Size     Functions\n\
           16     _RNvCs123_3foo3bar\n\
            8     malloc\n",
        )
        .unwrap();
        assert_eq!(parsed["_RNvCs123_3foo3bar"], 16);
        assert_eq!(parsed["malloc"], 8);
    }

    #[test]
    fn parses_call_graph() {
        let graph = parse_call_graph(
            "\
0000000000001000 <_RNvCs123_3foo3bar>:\n\
    1000:\tcallq\t0x2000 <_RNvCs123_3foo3baz>\n\
    1005:\tcallq\t*%rax\n\
0000000000002000 <_RNvCs123_3foo3baz>:\n\
    2000:\tretq\n",
        );
        let calls = &graph["_RNvCs123_3foo3bar"];
        assert_eq!(calls.direct, vec!["_RNvCs123_3foo3baz"]);
        assert_eq!(calls.indirect_sites, 1);
    }

    #[test]
    fn computes_known_stack_path() {
        let program = Program {
            frames: HashMap::from([
                ("a".to_owned(), 16),
                ("b".to_owned(), 24),
                ("c".to_owned(), 8),
            ]),
            graph: HashMap::from([
                (
                    "a".to_owned(),
                    FunctionCalls {
                        direct: vec!["b".to_owned(), "c".to_owned()],
                        indirect_sites: 0,
                    },
                ),
                ("b".to_owned(), FunctionCalls::default()),
                ("c".to_owned(), FunctionCalls::default()),
            ]),
        };
        let mut analysis = Analysis::new(&program);
        assert_eq!(analysis.known_stack_bytes("a"), 40);
    }
}
