#[cfg(test)]
mod json_benchmarks {
    use crate::helpers::configuration::Config;
    use crate::serdes::json::SerdeJson;
    use crate::serdes::optimized_json::OptimizedJsonParser;
    use std::time::{Duration, Instant};

    /// Helper function to benchmark a parsing function
    fn benchmark<F, R>(name: &str, iterations: usize, f: F) -> Duration
    where
        F: Fn() -> R,
    {
        // Warm-up
        for _ in 0..5 {
            f();
        }

        let start = Instant::now();
        for _ in 0..iterations {
            f();
        }
        let elapsed = start.elapsed();

        println!(
            "{}: {:?} ({:?} per iteration)",
            name,
            elapsed,
            elapsed / iterations as u32
        );

        elapsed
    }

    // Test data - Standard JSON
    const STANDARD_JSON: &str = r#"{"name":"test","value":42,"items":[1,2,3]}"#;

    // Test data - Nested complex JSON
    const COMPLEX_NESTED_JSON: &str = r#"{
      "metadata": {
        "version": "1.0",
        "generated": "2023-01-01T12:00:00Z",
        "source": "test-data"
      },
      "records": [
        {
          "id": 1,
          "name": "Record 1",
          "attributes": {
            "color": "red",
            "size": "large",
            "tags": ["important", "critical", "production"]
          },
          "metrics": {
            "value": 42.5,
            "count": 100,
            "ratios": [0.1, 0.2, 0.3, 0.4]
          }
        },
        {
          "id": 2,
          "name": "Record 2",
          "attributes": {
            "color": "blue",
            "size": "medium",
            "tags": ["normal", "development"]
          },
          "metrics": {
            "value": 18.25,
            "count": 50,
            "ratios": [0.5, 0.5]
          }
        }
      ],
      "statistics": {
        "total_records": 2,
        "average_value": 30.375,
        "distribution": {
          "red": 1,
          "blue": 1
        }
      }
    }"#;

    // Test data - Array of records
    const ARRAY_OF_RECORDS_JSON: &str = r#"[
      {"id": 1, "name": "Item 1", "value": 10.5},
      {"id": 2, "name": "Item 2", "value": 20.75},
      {"id": 3, "name": "Item 3", "value": 30.25},
      {"id": 4, "name": "Item 4", "value": 40.0},
      {"id": 5, "name": "Item 5", "value": 50.5}
    ]"#;

    // Test data - JSON with single quotes (requiring processing)
    const SINGLE_QUOTE_JSON: &str = r#"{'name':'test','value':42,'items':[1,2,3]}"#;

    // Test data - JSON with unicode markers (requiring processing)
    const UNICODE_JSON: &str = r#"{"name":u'unicode text',"value":42}"#;

    // Test data - Concatenated JSON objects
    const CONCATENATED_JSON: &str =
        r#"{"name":"first","value":1}{"name":"second","value":2}{"name":"third","value":3}"#;

    // Test data - Very large nested structure
    const LARGE_NESTED_JSON: &str = r#"{
      "metadata": {
        "version": "1.0",
        "generated": "2023-01-01T12:00:00Z",
        "source": "large-test-data"
      },
      "records": [
        {
          "id": 1,
          "name": "Record 1",
          "attributes": {
            "color": "red",
            "size": "large",
            "tags": ["important", "critical", "production"]
          },
          "metrics": {
            "value": 42.5,
            "count": 100,
            "ratios": [0.1, 0.2, 0.3, 0.4]
          },
          "details": {
            "created_at": "2023-01-01T00:00:00Z",
            "updated_at": "2023-01-02T00:00:00Z",
            "status": "active",
            "flags": ["reviewed", "approved", "published"],
            "history": [
              {"timestamp": "2023-01-01T00:00:00Z", "action": "created", "user": "system"},
              {"timestamp": "2023-01-01T01:00:00Z", "action": "updated", "user": "admin"},
              {"timestamp": "2023-01-01T02:00:00Z", "action": "published", "user": "editor"}
            ]
          }
        },
        {
          "id": 2,
          "name": "Record 2",
          "attributes": {
            "color": "blue",
            "size": "medium",
            "tags": ["normal", "development"]
          },
          "metrics": {
            "value": 18.25,
            "count": 50,
            "ratios": [0.5, 0.5]
          },
          "details": {
            "created_at": "2023-01-01T10:00:00Z",
            "updated_at": "2023-01-02T10:00:00Z",
            "status": "inactive",
            "flags": ["reviewed"],
            "history": [
              {"timestamp": "2023-01-01T10:00:00Z", "action": "created", "user": "system"},
              {"timestamp": "2023-01-01T11:00:00Z", "action": "updated", "user": "user1"}
            ]
          }
        }
      ],
      "statistics": {
        "total_records": 2,
        "average_value": 30.375,
        "distribution": {
          "red": 1,
          "blue": 1
        }
      }
    }"#;

    // This function benchmarks the original implementation
    #[test]
    #[ignore] // Only run when explicitly requested
    fn benchmark_original_implementation() {
        // Set up environment for special processing
        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "true");
        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "true");

        // Benchmark with standard JSON (100000 iterations)
        benchmark("Original - Standard JSON", 100000, || {
            SerdeJson::deserialize(STANDARD_JSON)
        });

        // Benchmark with complex nested JSON (10000 iterations)
        benchmark("Original - Complex Nested JSON", 10000, || {
            SerdeJson::deserialize(COMPLEX_NESTED_JSON)
        });

        // Benchmark with array of records (50000 iterations)
        benchmark("Original - Array of Records", 50000, || {
            SerdeJson::deserialize(ARRAY_OF_RECORDS_JSON)
        });

        // Benchmark with single quote JSON (50000 iterations)
        benchmark("Original - Single Quote JSON", 50000, || {
            SerdeJson::deserialize(SINGLE_QUOTE_JSON)
        });

        // Benchmark with Unicode JSON (50000 iterations)
        benchmark("Original - Unicode JSON", 50000, || {
            SerdeJson::deserialize(UNICODE_JSON)
        });

        // Benchmark with concatenated JSON (20000 iterations)
        benchmark("Original - Concatenated JSON", 20000, || {
            SerdeJson::deserialize(CONCATENATED_JSON)
        });

        // Benchmark with large nested JSON (5000 iterations)
        benchmark("Original - Large Nested JSON", 5000, || {
            SerdeJson::deserialize(LARGE_NESTED_JSON)
        });

        // Clean up environment variables
        std::env::remove_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING");
        std::env::remove_var("SKIPPR_ENABLE_UNICODE_PARSING");
    }

    // This function benchmarks the optimized implementation
    #[test]
    #[ignore] // Only run when explicitly requested
    fn benchmark_optimized_implementation() {
        // Set up environment for special processing
        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "true");
        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "true");

        // Create parser instances
        let enable_sq = Config::get_enable_single_quote_parsing();
        let enable_unicode = Config::get_enable_unicode_parsing();
        let parser = OptimizedJsonParser::new(enable_sq, enable_unicode);

        // Benchmark with standard JSON (100000 iterations)
        benchmark("Optimized - Standard JSON", 100000, || {
            parser.parse(STANDARD_JSON)
        });

        // Benchmark with complex nested JSON (10000 iterations)
        benchmark("Optimized - Complex Nested JSON", 10000, || {
            parser.parse(COMPLEX_NESTED_JSON)
        });

        // Benchmark with array of records (50000 iterations)
        benchmark("Optimized - Array of Records", 50000, || {
            parser.parse(ARRAY_OF_RECORDS_JSON)
        });

        // Benchmark with single quote JSON (50000 iterations)
        benchmark("Optimized - Single Quote JSON", 50000, || {
            parser.parse(SINGLE_QUOTE_JSON)
        });

        // Benchmark with Unicode JSON (50000 iterations)
        benchmark("Optimized - Unicode JSON", 50000, || {
            parser.parse(UNICODE_JSON)
        });

        // Benchmark with concatenated JSON (20000 iterations)
        benchmark("Optimized - Concatenated JSON", 20000, || {
            parser.parse(CONCATENATED_JSON)
        });

        // Benchmark with large nested JSON (5000 iterations)
        benchmark("Optimized - Large Nested JSON", 5000, || {
            parser.parse(LARGE_NESTED_JSON)
        });

        // Clean up environment variables
        std::env::remove_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING");
        std::env::remove_var("SKIPPR_ENABLE_UNICODE_PARSING");
    }

    // Compare both implementations side by side
    #[test]
    #[ignore] // Only run when explicitly requested
    fn benchmark_comparison() {
        // Set up environment for special processing
        std::env::set_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING", "true");
        std::env::set_var("SKIPPR_ENABLE_UNICODE_PARSING", "true");

        // Create parser instance
        let enable_sq = Config::get_enable_single_quote_parsing();
        let enable_unicode = Config::get_enable_unicode_parsing();
        let parser = OptimizedJsonParser::new(enable_sq, enable_unicode);

        println!("\n==== BENCHMARK COMPARISON ====\n");

        // Standard JSON comparison
        println!("--- Standard JSON ---");
        let orig_time = benchmark("Original", 10000, || SerdeJson::deserialize(STANDARD_JSON));

        let opt_time = benchmark("Optimized", 10000, || parser.parse(STANDARD_JSON));

        let improvement =
            (orig_time.as_nanos() as f64 / opt_time.as_nanos() as f64) * 100.0 - 100.0;
        println!("Improvement: {:.2}%\n", improvement);

        // Complex nested JSON comparison
        println!("--- Complex Nested JSON ---");
        let orig_time = benchmark("Original", 5000, || {
            SerdeJson::deserialize(COMPLEX_NESTED_JSON)
        });

        let opt_time = benchmark("Optimized", 5000, || parser.parse(COMPLEX_NESTED_JSON));

        let improvement =
            (orig_time.as_nanos() as f64 / opt_time.as_nanos() as f64) * 100.0 - 100.0;
        println!("Improvement: {:.2}%\n", improvement);

        // Single quote JSON comparison
        println!("--- Single Quote JSON ---");
        let orig_time = benchmark("Original", 10000, || {
            SerdeJson::deserialize(SINGLE_QUOTE_JSON)
        });

        let opt_time = benchmark("Optimized", 10000, || parser.parse(SINGLE_QUOTE_JSON));

        let improvement =
            (orig_time.as_nanos() as f64 / opt_time.as_nanos() as f64) * 100.0 - 100.0;
        println!("Improvement: {:.2}%\n", improvement);

        // Unicode JSON comparison
        println!("--- Unicode JSON ---");
        let orig_time = benchmark("Original", 10000, || SerdeJson::deserialize(UNICODE_JSON));

        let opt_time = benchmark("Optimized", 10000, || parser.parse(UNICODE_JSON));

        let improvement =
            (orig_time.as_nanos() as f64 / opt_time.as_nanos() as f64) * 100.0 - 100.0;
        println!("Improvement: {:.2}%\n", improvement);

        // Concatenated JSON comparison
        println!("--- Concatenated JSON ---");
        let orig_time = benchmark("Original", 5000, || {
            SerdeJson::deserialize(CONCATENATED_JSON)
        });

        let opt_time = benchmark("Optimized", 5000, || parser.parse(CONCATENATED_JSON));

        let improvement =
            (orig_time.as_nanos() as f64 / opt_time.as_nanos() as f64) * 100.0 - 100.0;
        println!("Improvement: {:.2}%\n", improvement);

        // Large nested JSON comparison
        println!("--- Large Nested JSON ---");
        let orig_time = benchmark("Original", 1000, || {
            SerdeJson::deserialize(LARGE_NESTED_JSON)
        });

        let opt_time = benchmark("Optimized", 1000, || parser.parse(LARGE_NESTED_JSON));

        let improvement =
            (orig_time.as_nanos() as f64 / opt_time.as_nanos() as f64) * 100.0 - 100.0;
        println!("Improvement: {:.2}%\n", improvement);

        // Clean up environment variables
        std::env::remove_var("SKIPPR_ENABLE_SINGLE_QUOTE_PARSING");
        std::env::remove_var("SKIPPR_ENABLE_UNICODE_PARSING");
    }
}
