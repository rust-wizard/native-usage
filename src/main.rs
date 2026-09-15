use anyhow::{bail, Context, Result};  
use clap::Parser;  
use std::{  
    collections::HashMap,  
    fs::File,  
    io::Write,  
    path::PathBuf,  
    process::Command,  
};  
use sui_json_rpc_types::CheckpointId;  
use sui_sdk::SuiClientBuilder;  
use move_trace_format::format::{MoveTraceReader, TraceEvent};  
  
#[derive(Parser, Debug)]  
#[command(name = "native-usage")]  
#[command(about = "Statistics on native function call frequency over a checkpoint range")]  
struct Args {   
    #[arg(long, default_value = "1000")]  
    start: u64,  
  
    #[arg(long, default_value = "1000")]  
    end: u64,  
  
    #[arg(long, default_value = "https://fullnode.testnet.sui.io:443")]  
    rpc: String,  
   
    #[arg(long, default_value = "sui replay")]  
    replay_bin: String,  
    
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
    let client = SuiClientBuilder::default()  
        .build(&args.rpc)  
        .await  
        .context("failed to build SuiClient")?;  
  
    let mut all_digests = Vec::new();  
    for seq in args.start..=args.end {  
        let checkpoint = client  
            .read_api()  
            .get_checkpoint(CheckpointId::SequenceNumber(seq))  
            .await  
            .with_context(|| format!("failed to fetch checkpoint {seq}"))?;  
  
        if checkpoint.sequence_number != seq {  
            eprintln!(  
                "warning: requested checkpoint {seq} but got {}",  
                checkpoint.sequence_number  
            );  
        }  
  
        for digest in checkpoint.transactions.iter() {  
            all_digests.push(digest.to_string());  
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
    let status = Command::new(&args.replay_bin)  
        .arg("--digests-path")  
        .arg(digest_file)  
        .arg("--trace")
        .arg("--output-dir")  
        .arg(trace_root)  
        .arg("--show-effects")  
        .status()  
        .with_context(|| format!("failed to spawn `{}`", args.replay_bin))?;  
  
    if !status.success() {  
        bail!("sui replay exited with status {status}");  
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
                    let key = format!("{}::{}", frame.module, frame.function_name);  
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
