//! Integration tests for hermes-compress trajectory compression.

#[test]
fn test_trajectory_compressor_new() {
    let config = hermes_compress::CompressionConfig::default();
    let compressor = hermes_compress::TrajectoryCompressor::new(config);
    assert_eq!(compressor.count_tokens("hello world"), 2);
}

#[test]
fn test_aggregate_metrics_default() {
    let metrics = hermes_compress::AggregateMetrics::default();
    assert_eq!(metrics.avg_compression_ratio(), 1.0);
}

#[test]
fn test_summarizer_new() {
    let config = hermes_compress::CompressionConfig::default();
    let _summarizer = hermes_compress::Summarizer::new(&config);
}
