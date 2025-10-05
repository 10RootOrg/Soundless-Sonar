use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use crate::{Config, Logger};

pub fn run_enrich(config: &Config, logger: Arc<Logger>) -> Result<()> {
    logger.info("Starting enrich mode")?;

    // Validate input parameters
    if config.enrich_song_path.is_empty() {
        anyhow::bail!("Song path is required for enrich mode. Use --song-path <PATH>");
    }

    let input_path = Path::new(&config.enrich_song_path);
    if !input_path.exists() {
        anyhow::bail!("Input song file does not exist: {}", config.enrich_song_path);
    }

    // Validate ffmpeg path
    let ffmpeg_path = Path::new(&config.ffmpeg_path);
    if !ffmpeg_path.exists() {
        anyhow::bail!("FFmpeg executable not found at: {}", config.ffmpeg_path);
    }

    // Generate output filename (input without extension + "_3pings.flac")
    let output_path = generate_output_path(input_path)?;

    logger.info(&format!("Input file: {}", config.enrich_song_path))?;
    logger.info(&format!("Output file: {}", output_path))?;
    logger.info(&format!("Interval length: {:.2}s", config.enrich_interval_length_s))?;
    logger.info(&format!("Ping length: {:.2}s", config.enrich_ping_length_s))?;

    // Build the FFmpeg command
    let result = run_ffmpeg_command(config, &output_path, logger.clone());

    match result {
        Ok(_) => {
            logger.info("Enrich processing completed successfully")?;
            println!("✓ Audio file enriched with sonar pings");
            println!("  Output: {}", output_path);
        }
        Err(e) => {
            logger.error(&format!("Enrich processing failed: {}", e))?;
            anyhow::bail!("FFmpeg processing failed: {}", e);
        }
    }

    Ok(())
}

fn generate_output_path(input_path: &Path) -> Result<String> {
    let stem = input_path
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("Could not extract filename stem"))?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid filename encoding"))?;
    
    let parent_dir = input_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    
    let output_path = parent_dir.join(format!("{}_3pings.flac", stem));
    
    Ok(output_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Could not convert output path to string"))?
        .to_string())
}

fn run_ffmpeg_command(config: &Config, output_path: &str, logger: Arc<Logger>) -> Result<()> {
    logger.info("Executing FFmpeg command...")?;

    // Build the filter complex string
    let filter_complex = format!(
        "[0:a]aresample=48000,aformat=sample_rates=48000:channel_layouts=stereo[a];aevalsrc=exprs='(lt(mod(t,{}),{}))*pow(10,-35/20)*sin(2*PI*18500*t)':s=48000:d=999999:channel_layout=stereo[u];[a][u]amix=inputs=2:duration=first:dropout_transition=0[out]",
        config.enrich_interval_length_s,
        config.enrich_ping_length_s
    );

    logger.info(&format!("Filter complex: {}", filter_complex))?;

    // Execute FFmpeg command
    let mut command = Command::new(&config.ffmpeg_path);
    command
        .arg("-hide_banner")
        .arg("-i")
        .arg(&config.enrich_song_path)
        .arg("-filter_complex")
        .arg(&filter_complex)
        .arg("-map")
        .arg("[out]")
        .arg("-c:a")
        .arg("flac")
        .arg("-map_metadata")
        .arg("0")
        .arg(output_path);

    logger.info(&format!("Command: {:?}", command))?;

    // Execute and capture output
    let output = command.output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        
        logger.error(&format!("FFmpeg stderr: {}", stderr))?;
        logger.error(&format!("FFmpeg stdout: {}", stdout))?;
        
        anyhow::bail!(
            "FFmpeg failed with exit code {:?}\nStderr: {}\nStdout: {}", 
            output.status.code(), 
            stderr, 
            stdout
        );
    }

    // Log successful completion
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        logger.info(&format!("FFmpeg output: {}", stdout))?;
    }

    Ok(())
}