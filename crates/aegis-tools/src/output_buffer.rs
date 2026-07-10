use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;

const LARGE_OUTPUT_THRESHOLD: usize = 64 * 1024; // 64KB

/// If output is larger than 64KB, write to /tmp/aegis-outputs/{hash}.txt
/// and return a summary with path reference.
pub fn guard_output_size(output: String) -> String {
    if output.len() <= LARGE_OUTPUT_THRESHOLD {
        return output;
    }

    let mut hasher = DefaultHasher::new();
    output.hash(&mut hasher);
    let hash = hasher.finish();

    let dir = std::path::PathBuf::from("/tmp/aegis-outputs");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create output buffer dir: {e}");
        return format!(
            "{}...\n[output truncated: {} bytes total, buffer dir unavailable]",
            &output[..LARGE_OUTPUT_THRESHOLD],
            output.len()
        );
    }

    let path = dir.join(format!("{hash:016x}.txt"));
    match std::fs::File::create(&path).and_then(|mut f| f.write_all(output.as_bytes())) {
        Ok(_) => format!(
            "[Large output ({} bytes) saved to: {}]\nFirst 2KB preview:\n{}",
            output.len(),
            path.display(),
            &output[..output.floor_char_boundary(2048)]
        ),
        Err(e) => {
            tracing::warn!("Failed to write output buffer: {e}");
            format!(
                "{}...\n[output truncated: {} bytes total]",
                &output[..LARGE_OUTPUT_THRESHOLD],
                output.len()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_output_passes_through() {
        let output = "hello world".to_string();
        assert_eq!(guard_output_size(output.clone()), output);
    }

    #[test]
    fn test_empty_output() {
        assert_eq!(guard_output_size(String::new()), "");
    }

    #[test]
    fn test_exactly_threshold_passes_through() {
        let output = "a".repeat(LARGE_OUTPUT_THRESHOLD);
        let result = guard_output_size(output.clone());
        assert_eq!(result, output);
    }

    #[test]
    fn test_large_output_writes_to_file() {
        let output = "x".repeat(LARGE_OUTPUT_THRESHOLD + 1000);
        let result = guard_output_size(output);
        assert!(result.contains("Large output"));
        assert!(result.contains("saved to"));
        assert!(result.contains("preview"));
    }

    #[test]
    fn test_output_at_boundary_not_truncated() {
        let output = "b".repeat(100);
        assert_eq!(guard_output_size(output.clone()), output);
    }
}
