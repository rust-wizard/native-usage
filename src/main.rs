use anyhow::{bail, Context, Result};  
use clap::Parser;  
use std::{  
    collections::HashMap,  
    fs::File,  
    io::Write,  
    path::PathBuf,  
    process::Command,  
};  
use move_trace_format::format::{MoveTraceReader, TraceEvent};  
use sui_graphql::Client as GraphQlClient;
  
#[derive(Parser, Debug)]  
#[command(name = "native-usage")]  
#[command(about = "Statistics on native function call frequency over a checkpoint range")]  
struct Args {   
    #[arg(long, default_value = "322724832")]  
    start: u64,  
  
    #[arg(long, default_value = "322724832")]  
    end: u64,  
  
    #[arg(long, default_value = "https://graphql.mainnet.sui.io/graphql")]
    graphql: String,
   
    #[arg(long, default_value = "sui")]
    sui_bin: String,
    
    #[arg(long, default_value = "trace")]  
    out_dir: PathBuf,  
}  
  
#[tokio::main]  
async fn main() -> Result<()> {  
    let args = Args::parse();  
    std::fs::create_dir_all(&args.out_dir)?;  
   
    let digests = collect_digests(&args).await?;  
    let digest_file = args.out_dir.join("tx_digests.txt");  
    write_digest_file(&digest_file, &digests)?;  
    println!(  
        "Collected {} transaction digests from checkpoints [{}, {}] -> {}",  
        digests.len(),  
        args.start,  
        args.end,  
        digest_file.display()  
    );  
   
    let trace_root = args.out_dir.join("traces");  
    std::fs::create_dir_all(&trace_root)?;  
    run_replay_with_trace(&args, &digest_file, &trace_root)?;  
  
    let stats = compute_native_stats(&trace_root)?;  
    print_stats(&stats);  
    write_stats_json(&args.out_dir.join("native_stats.json"), &stats)?;  
  
    Ok(())  
}  
 
async fn collect_digests(args: &Args) -> Result<Vec<String>> {  
    let client = GraphQlClient::new(&args.graphql)
        .context("failed to create Sui GraphQL client")?;
  
    let mut all_digests = Vec::new();  
    for seq in args.start..=args.end {  
        let checkpoint = client  
            .get_checkpoint(Some(seq))
            .await  
            .with_context(|| format!("failed to fetch checkpoint {seq}"))?;  

        let checkpoint = checkpoint
            .with_context(|| format!("checkpoint {seq} was not found"))?;
  
        if checkpoint.summary.sequence_number != seq {
            eprintln!(  
                "warning: requested checkpoint {seq} but got {}",  
                checkpoint.summary.sequence_number
            );  
        }  
  
        for transaction in checkpoint.contents.transactions() {
            all_digests.push(transaction.transaction().to_string());
        }  
  
        if seq % 100 == 0 {  
            println!("processed checkpoint {seq}, total digests so far: {}", all_digests.len());  
        }  
    }  
    Ok(all_digests)  
}  
 
fn write_digest_file(path: &PathBuf, digests: &[String]) -> Result<()> {  
    let mut f = File::create(path)?;  
    for d in digests {  
        writeln!(f, "{d}")?;  
    }  
    Ok(())  
}  
 
fn run_replay_with_trace(args: &Args, digest_file: &PathBuf, trace_root: &PathBuf) -> Result<()> {  
    let status = Command::new(&args.sui_bin)  
        .arg("replay")
        .arg("--digests-path")  
        .arg(digest_file)  
        .arg("--trace")
        .arg("--output-dir")  
        .arg(trace_root)  
        .arg("--show-effects")
        .arg("false")
        .status()  
        .with_context(|| format!("failed to spawn `{}`", args.sui_bin))?;  
  
    if !status.success() {  
        bail!("sui replay exited with status {status}");  
    } 
    cleanup_non_trace_artifacts(trace_root)  
        .context("failed to clean up non-trace artifacts")?; 
    Ok(())  
}

fn cleanup_non_trace_artifacts(trace_root: &std::path::Path) -> Result<()> {  
    const KEEP_FILE: &str = "trace.json.zst";  
  
    let tx_dirs = std::fs::read_dir(trace_root)  
        .with_context(|| format!("failed to read trace root {}", trace_root.display()))?;  
  
    for entry in tx_dirs {  
        let entry = entry?;  
        let path = entry.path();  
        if !path.is_dir() {  
            // Stray file directly under trace_root; not one of our per-tx directories, skip it.  
            continue;  
        }  
  
        for child in std::fs::read_dir(&path)  
            .with_context(|| format!("failed to read tx output dir {}", path.display()))?  
        {  
            let child = child?;  
            let child_path = child.path();  
            let file_type = child.file_type()?;  
  
            let is_keep_file = file_type.is_file()  
                && child_path.file_name().and_then(|n| n.to_str()) == Some(KEEP_FILE);  
  
            if is_keep_file {  
                continue;  
            }  
  
            if file_type.is_dir() {  
                std::fs::remove_dir_all(&child_path).with_context(|| {  
                    format!("failed to remove directory {}", child_path.display())  
                })?;  
            } else {  
                std::fs::remove_file(&child_path).with_context(|| {  
                    format!("failed to remove file {}", child_path.display())  
                })?;  
            }  
        }  
    }  
  
    Ok(())  
}
  
#[derive(Debug, Default, Clone, serde::Serialize)]  
struct NativeStat {  
    name: String,  
    total_calls: u64,  
    percentage: f64,  
}  
 
fn compute_native_stats(trace_root: &PathBuf) -> Result<Vec<NativeStat>> {  
    let mut counts: HashMap<String, u64> = HashMap::new();  
    let mut total_native_calls: u64 = 0;  
  
    for entry in walkdir::WalkDir::new(trace_root)  
        .into_iter()  
        .filter_map(|e| e.ok())  
        .filter(|e| e.file_type().is_file())  
        .filter(|e| e.file_name().to_string_lossy().ends_with("trace.json.zst"))  
    {  
        let path = entry.path().to_path_buf();  
        let file = File::open(&path)  
            .with_context(|| format!("failed to open trace file {}", path.display()))?;  
        let reader = MoveTraceReader::new(file)  
            .with_context(|| format!("failed to read trace file {}", path.display()))?;  
  
        for event in reader {  
            let event = match event {  
                Ok(e) => e,  
                Err(err) => {  
                    eprintln!("warning: skipping malformed event in {}: {err}", path.display());  
                    continue;  
                }  
            };  
            if let TraceEvent::OpenFrame { frame, .. } = event {  
                if frame.is_native {  
                    // let key = format!("{}::{}", frame.module, frame.function_name);  
                    let key = frame.function_name;
                    *counts.entry(key).or_insert(0) += 1;  
                    total_native_calls += 1;  
                }  
            }  
        }  
    }  
  
    if total_native_calls == 0 {  
        return Ok(Vec::new());  
    }  
  
    let mut stats: Vec<NativeStat> = counts  
        .into_iter()  
        .map(|(name, total_calls)| NativeStat {  
            name,  
            total_calls,  
            percentage: 100.0 * (total_calls as f64) / (total_native_calls as f64),  
        })  
        .collect();  
   
    stats.sort_by(|a, b| b.total_calls.cmp(&a.total_calls));  
    Ok(stats)  
}  
  
fn print_stats(stats: &[NativeStat]) {  
    println!("{:<50} {:>12} {:>10}", "native function", "total_calls", "pct(%)");  
    for s in stats {  
        println!("{:<50} {:>12} {:>9.2}", s.name, s.total_calls, s.percentage);  
    }  
}  
  
fn write_stats_json(path: &PathBuf, stats: &[NativeStat]) -> Result<()> {  
    let file = File::create(path)?;  
    serde_json::to_writer_pretty(file, stats)?;  
    Ok(())  
}
