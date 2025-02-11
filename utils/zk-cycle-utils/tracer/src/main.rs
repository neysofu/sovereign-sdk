use std::process::Command;
use std::str;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::read_to_string;
use prettytable::{Table, Row, Cell, format};
use textwrap::wrap;
use indicatif::{ProgressBar, ProgressStyle};
use goblin::elf::{Elf, sym::STT_FUNC};
use rustc_demangle::demangle;
use regex::Regex;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = 30)]
    /// Include the "top" number of functions
    top: usize,

    #[arg(long)]
    /// Don't print stack aware instruction counts
    no_stack_counts: bool,

    #[arg(long)]
    /// Don't print raw (stack un-aware) instruction counts
    no_raw_counts: bool,

    #[arg(long, required=true)]
    /// Path to the riscv32 elf
    rollup_elf: String,

    #[arg(long, required=true)]
    /// Path to the rollup trace.
    /// File must be one u64 program counter per line
    rollup_trace: String,

    #[arg(short, long)]
    /// Strip the hashes from the function name while printing
    strip_hashes: bool,
}

fn strip_hash(name_with_hash: &str) -> String {
    let re = Regex::new(r"::h[0-9a-fA-F]+$").unwrap();
    re.replace(name_with_hash, "").to_string()
}

fn print_intruction_counts(count_vec: Vec<(&String, &usize)>, top_n: usize, strip_hashes: bool) {
    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_DEFAULT);
    table.set_titles(Row::new(vec![
        Cell::new("Function Name"),
        Cell::new("Instruction Count"),
    ]));

    let wrap_width = 90;
    let mut row_count = 0;
    for (key, value) in count_vec {
        let mut stripped_key = key.clone();
        if strip_hashes {
            stripped_key = strip_hash(&key);
        }
        row_count+=1;
        if row_count > top_n {
            break;
        }
        let wrapped_key = wrap(&stripped_key, wrap_width);
        let key_cell_content = wrapped_key.join("\n");
        table.add_row(Row::new(vec![
            Cell::new(&key_cell_content),
            Cell::new(&value.to_string()),
        ]));
    }

    table.printstd();
}

fn _build_lookups_radare_2(
    start_lookup: &mut HashMap<u64, String>,
    end_lookup: &mut HashMap<u64, String>,
    func_range_lookup: &mut HashMap<String, (u64, u64)>,
    elf_name: &str
) -> std::io::Result<()>  {
    let output = Command::new("r2")
        .arg("-q")
        .arg("-c")
        .arg("aa;afl")
        .arg(elf_name)
        .output()?;

    if output.status.success() {
        let result_str = str::from_utf8(&output.stdout).unwrap();
        for line in result_str.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let address = u64::from_str_radix(&parts[0][2..], 16).unwrap();
            let size = parts[2].parse::<u64>().unwrap();
            let end_address = address + size - 4;
            let function_name = parts[3];
            start_lookup.insert(address, function_name.to_string());
            end_lookup.insert(end_address, function_name.to_string());
            func_range_lookup.insert(function_name.to_string(), (address,end_address));
        }
    } else {
        eprintln!("Error executing command: {}", str::from_utf8(&output.stderr).unwrap());
    }
    Ok(())
}

fn build_goblin_lookups(
    start_lookup: &mut HashMap<u64, String>,
    end_lookup: &mut HashMap<u64, String>,
    func_range_lookup: &mut HashMap<String, (u64, u64)>,
    elf_name: &str
) -> std::io::Result<()>  {
    let buffer = std::fs::read(elf_name).unwrap();
    let elf = Elf::parse(&buffer).unwrap();

    for sym in &elf.syms {
        if sym.st_type() == STT_FUNC {
            let name = elf.strtab.get(sym.st_name).unwrap_or(Ok("")).unwrap_or("");
            let demangled_name = demangle(name);
            let size = sym.st_size;
            let start_address = sym.st_value;
            let end_address = start_address + size - 4;
            start_lookup.insert(start_address, demangled_name.to_string());
            end_lookup.insert(end_address, demangled_name.to_string());
            func_range_lookup.insert(demangled_name.to_string(), (start_address,end_address));
        }
    }
    Ok(())
}

fn increment_stack_counts(instruction_counts: &mut HashMap<String, usize>, function_stack: &[String]) {
    for function_name in function_stack {
        *instruction_counts.entry(function_name.clone()).or_insert(0) += 1;
    }
}

fn main() -> std::io::Result<()> {

    let args = Args::parse();
    let top_n = args.top;
    let rollup_elf_path  = args.rollup_elf;
    let rollup_trace_path = args.rollup_trace;
    let no_stack_counts = args.no_stack_counts;
    let no_raw_counts = args.no_raw_counts;
    let strip_hashes = args.strip_hashes;

    let mut start_lookup = HashMap::new();
    let mut end_lookup = HashMap::new();
    let mut func_range_lookup = HashMap::new();
    build_goblin_lookups(&mut start_lookup, &mut end_lookup, &mut func_range_lookup, &rollup_elf_path).unwrap();

    let mut function_ranges: Vec<(u64, u64, String)> = func_range_lookup
        .iter()
        .map(|(function_name, &(start, end))| (start, end, function_name.clone()))
        .collect();

    function_ranges.sort_by_key(|&(start, _, _)| start);

    let file_content = read_to_string(&rollup_trace_path).unwrap();
    let mut function_stack: Vec<String> = Vec::new();
    let mut instruction_counts: HashMap<String, usize> = HashMap::new();
    let mut counts_without_callgraph: HashMap<String, usize> = HashMap::new();
    let total_lines = file_content.lines().count() as u64;
    let mut current_function_range : (u64,u64) = (0,0);

    let update_interval = 1000usize;
    let pb = ProgressBar::new(total_lines);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})").unwrap()
        .progress_chars("#>-"));


    for (c,line) in file_content.lines().enumerate() {
        if c % &update_interval == 0 {
            pb.inc(update_interval as u64);
        }
        let pc = line.parse().unwrap();

        // Raw counts without considering the callgraph at all
        // we're just checking if the PC belongs to a function
        // if so we're incrementing. This would ignore the call stack
        // so for example "main" would only have a hundred instructions or so
        if let Ok(index) = function_ranges.binary_search_by(
            |&(start, end, _)| {
                if pc < start {
                    Ordering::Greater
                } else if pc > end {
                    Ordering::Less
                } else {
                    Ordering::Equal
                } })
        {
            let (_, _, fname) = &function_ranges[index];
            *counts_without_callgraph.entry(fname.clone()).or_insert(0) += 1;
        } else {
            *counts_without_callgraph.entry("anonymous".to_string()).or_insert(0) += 1;
        }

        // The next section considers the callstack
        // We build a callstack and maintain it based on some rules
        // Functions lower in the stack get their counts incremented

        // we are still in the current function
        if pc > current_function_range.0 && pc <= current_function_range.1 {
            increment_stack_counts(&mut instruction_counts, &function_stack);
            continue;
        }

        // jump to a new function (or the same one)
        if let Some(function_name) = start_lookup.get(&pc) {
            increment_stack_counts(&mut instruction_counts, &function_stack);
            // jump to a new function (not recursive)
            if !function_stack.contains(&function_name) {
                function_stack.push(function_name.clone());
                current_function_range = *func_range_lookup.get(function_name).unwrap();
            }
        } else {
            // this means pc now points to an instruction that is
            // 1. not in the current function's range
            // 2. not a new function call
            // we now account for a new possibility where we're returning to a function in the stack
            // this need not be the immediate parent and can be any of the existing functions in the stack
            // due to some optimizations that the compiler can make
            let mut unwind_point = 0;
            let mut unwind_found = false;
            for (c,f) in function_stack.iter().enumerate() {
                let (s, e) = func_range_lookup.get(f).unwrap();
                if pc > *s && pc <=*e {
                    unwind_point = c;
                    unwind_found = true;
                    break
                }
            }
            // unwinding until the parent
            if unwind_found {

                function_stack.truncate(unwind_point + 1);
                increment_stack_counts(&mut instruction_counts, &function_stack);
                continue;
            }

            // if no unwind point has been found, that means we jumped to some random location
            // so we'll just increment the counts for everything in the stack
            increment_stack_counts(&mut instruction_counts, &function_stack);
        }

    }

    pb.finish_with_message("done");

    let mut raw_counts: Vec<(&String, &usize)> = instruction_counts.iter().collect();
    raw_counts.sort_by(|a, b| b.1.cmp(&a.1));

    println!("\n\nTotal instructions in trace: {}", total_lines);
    if !no_stack_counts {
        println!("\n\n Instruction counts considering call graph");
        print_intruction_counts(raw_counts, top_n, strip_hashes);
    }

    let mut raw_counts: Vec<(&String, &usize)> = counts_without_callgraph.iter().collect();
    raw_counts.sort_by(|a, b| b.1.cmp(&a.1));
    if !no_raw_counts {
        println!("\n\n Instruction counts ignoring call graph");
        print_intruction_counts(raw_counts, top_n, strip_hashes);
    }
    Ok(())
}
