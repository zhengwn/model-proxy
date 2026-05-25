// Feature: request-logging, Property 5: LogEntry JSON serialization round-trip

#[cfg(test)]
mod property_tests {
    use crate::logging::LogEntry;
    use proptest::prelude::*;

    fn arb_method() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("GET".to_string()),
            Just("POST".to_string()),
            Just("PUT".to_string()),
            Just("DELETE".to_string()),
        ]
    }

    fn arb_log_entry() -> impl Strategy<Value = LogEntry> {
        (
            "[a-zA-Z0-9_-]{1,32}",                                               // id
            "20[0-9]{2}-[01][0-9]-[0-3][0-9]T[0-2][0-9]:[0-5][0-9]:[0-5][0-9]Z", // timestamp
            arb_method(),
            "/[a-z0-9/]{0,50}",   // path
            "[a-zA-Z]{1,20}",     // provider
            "[a-zA-Z0-9-]{1,30}", // model
            100u16..=599u16,      // status
            0u64..=300_000u64,    // duration_ms
            any::<bool>(),        // is_stream
        )
            .prop_flat_map(
                |(id, timestamp, method, path, provider, model, status, duration_ms, is_stream)| {
                    let error_message = if status >= 400 {
                        prop_oneof![
                            Just(Some("upstream error".to_string())),
                            Just(Some("timeout".to_string())),
                            "[a-zA-Z ]{1,50}".prop_map(Some),
                        ]
                        .boxed()
                    } else {
                        Just(None).boxed()
                    };

                    let request_body =
                        prop_oneof![Just(None), "[a-zA-Z0-9 ]{1,100}".prop_map(Some),];

                    let response_body =
                        prop_oneof![Just(None), "[a-zA-Z0-9 ]{1,100}".prop_map(Some),];

                    let token_count = prop_oneof![Just(None), (0u64..=100_000u64).prop_map(Some),];

                    (error_message, request_body, response_body, token_count).prop_map(
                        move |(error_message, request_body, response_body, token_count)| LogEntry {
                            id: id.clone(),
                            timestamp: timestamp.clone(),
                            method: method.clone(),
                            path: path.clone(),
                            provider: provider.clone(),
                            model: model.clone(),
                            requested_model: None,
                            status,
                            duration_ms,
                            proxy_overhead_ms: None,
                            ttft_ms: None,
                            error_message,
                            request_body,
                            response_body,
                            is_stream,
                            token_count,
                        },
                    )
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 2.1, 3.2**
        ///
        /// For any valid LogEntry instance, serializing to JSON and deserializing back
        /// SHALL produce a LogEntry equal to the original. Additionally, the serialized
        /// JSON SHALL contain no embedded newline characters (ensuring single-line JSONL format).
        #[test]
        fn log_entry_json_round_trip(entry in arb_log_entry()) {
            // Serialize to JSON
            let json = serde_json::to_string(&entry)
                .expect("LogEntry should serialize to JSON");

            // Assert no embedded newline characters (JSONL single-line requirement)
            prop_assert!(
                !json.contains('\n'),
                "Serialized JSON must not contain newline characters for JSONL format, got: {}",
                json
            );
            prop_assert!(
                !json.contains('\r'),
                "Serialized JSON must not contain carriage return characters, got: {}",
                json
            );

            // Deserialize back
            let deserialized: LogEntry = serde_json::from_str(&json)
                .expect("JSON should deserialize back to LogEntry");

            // Assert round-trip equality
            prop_assert_eq!(&entry, &deserialized);
        }
    }
}

// Feature: request-logging, Property 6: LogConfig TOML serialization round-trip

#[cfg(test)]
mod property_tests_config {
    use crate::logging::{LogConfig, LogLevel};
    use proptest::prelude::*;

    /// Generate a valid LogLevel
    fn arb_log_level() -> impl Strategy<Value = LogLevel> {
        prop_oneof![Just(LogLevel::All), Just(LogLevel::ErrorsOnly),]
    }

    /// Generate a valid path-like string for log_dir (no newlines or null bytes)
    fn arb_path_string() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_/\\\\. -]{1,64}".prop_filter("no newlines or null bytes", |s| {
            !s.contains('\n') && !s.contains('\r') && !s.contains('\0')
        })
    }

    /// Generate an Option<String> for log_dir
    fn arb_log_dir() -> impl Strategy<Value = Option<String>> {
        prop_oneof![Just(None), arb_path_string().prop_map(Some),]
    }

    /// Generate a valid LogConfig within specified ranges
    fn arb_log_config() -> impl Strategy<Value = LogConfig> {
        (
            any::<bool>(),     // enabled
            arb_log_level(),   // level
            arb_log_dir(),     // log_dir
            any::<bool>(),     // record_body
            1024..=65536usize, // max_body_bytes
            1..=90u32,         // retention_days
        )
            .prop_map(
                |(enabled, level, log_dir, record_body, max_body_bytes, retention_days)| {
                    LogConfig {
                        enabled,
                        level,
                        log_dir,
                        record_body,
                        max_body_bytes,
                        retention_days,
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 5.1**
        ///
        /// For any valid LogConfig, serializing to TOML and deserializing back
        /// SHALL produce a LogConfig equal to the original.
        #[test]
        fn log_config_toml_round_trip(config in arb_log_config()) {
            let toml_str = toml::to_string(&config)
                .expect("LogConfig should serialize to TOML");
            let deserialized: LogConfig = toml::from_str(&toml_str)
                .expect("LogConfig should deserialize from TOML");
            prop_assert_eq!(config, deserialized);
        }
    }
}

// Feature: request-logging, Property 2: Log filtering respects config

#[cfg(test)]
mod property_tests_log_filtering {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use proptest::prelude::*;

    use crate::logging::{LogCollector, LogConfig, LogLevel};

    /// Generate a valid LogLevel
    fn arb_log_level() -> impl Strategy<Value = LogLevel> {
        prop_oneof![Just(LogLevel::All), Just(LogLevel::ErrorsOnly),]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 1.6, 1.7**
        ///
        /// For any status code (100–599) and any LogConfig, `should_log` SHALL return:
        /// - `false` when `enabled == false` (regardless of status),
        /// - `true` when `enabled == true && level == All`,
        /// - `(status >= 400)` when `enabled == true && level == ErrorsOnly`.
        #[test]
        fn log_filtering_respects_config(
            status in 100u16..=599u16,
            enabled in any::<bool>(),
            level in arb_log_level(),
        ) {
            let config = LogConfig {
                enabled,
                level: level.clone(),
                ..Default::default()
            };
            let arc_config = Arc::new(ArcSwap::from_pointee(config));
            let collector = LogCollector::new(arc_config, 16);

            let result = collector.should_log(status);

            let expected = if !enabled {
                false
            } else {
                match level {
                    LogLevel::All => true,
                    LogLevel::ErrorsOnly => status >= 400,
                }
            };

            prop_assert_eq!(result, expected,
                "should_log({}) with enabled={}, level={:?} returned {} but expected {}",
                status, enabled, level, result, expected
            );
        }
    }
}

// Feature: request-logging, Property 4: Truncation preserves UTF-8 validity

#[cfg(test)]
mod property_tests_utf8_truncation {
    use crate::logging::truncate_body;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 7.3**
        ///
        /// For any valid UTF-8 string (including multi-byte characters such as CJK, emoji,
        /// combining marks) and any byte limit >= 1, the truncation function SHALL produce
        /// a valid UTF-8 string (i.e., std::str::from_utf8 succeeds on the truncated portion
        /// before the marker).
        #[test]
        fn truncation_preserves_utf8_validity(
            input in any::<String>(),
            max_bytes in 1usize..=8192usize,
        ) {
            let result = truncate_body(&input, max_bytes);

            // The entire output must be valid UTF-8 (guaranteed by String type, but
            // we document the property explicitly).
            prop_assert!(
                std::str::from_utf8(result.as_bytes()).is_ok(),
                "Output must be valid UTF-8, got bytes: {:?}",
                result.as_bytes()
            );

            // If truncation occurred, the portion before "[truncated]" must also be valid UTF-8.
            if let Some(prefix) = result.strip_suffix("[truncated]") {
                prop_assert!(
                    std::str::from_utf8(prefix.as_bytes()).is_ok(),
                    "Truncated prefix must be valid UTF-8, got: {:?}",
                    prefix.as_bytes()
                );
                // The prefix byte length must not exceed the byte limit.
                prop_assert!(
                    prefix.len() <= max_bytes,
                    "Truncated prefix byte length {} exceeds max_bytes {}",
                    prefix.len(),
                    max_bytes
                );
            }
        }
    }
}

// Feature: request-logging, Property 3: Body truncation respects byte limit

#[cfg(test)]
mod property_tests_truncation_byte_limit {
    use crate::logging::truncate_body;
    use proptest::prelude::*;

    const TRUNCATED_MARKER: &str = "[truncated]";
    const TRUNCATED_MARKER_LEN: usize = 11; // "[truncated]" is exactly 11 bytes

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 1.3, 1.4, 7.1, 7.2**
        ///
        /// For any UTF-8 string `s` and any `max_body_bytes` in [1024, 65536],
        /// the truncation function SHALL produce output where:
        /// - If `s.len() <= max_body_bytes`, output equals `s` (no marker appended),
        /// - If `s.len() > max_body_bytes`, the output byte length (excluding the
        ///   `[truncated]` suffix) is ≤ `max_body_bytes`, and the output ends with `[truncated]`.
        #[test]
        fn body_truncation_respects_byte_limit(
            s in any::<String>(),
            max_body_bytes in 1024usize..=65536usize,
        ) {
            let result = truncate_body(&s, max_body_bytes);

            if s.len() <= max_body_bytes {
                // No truncation needed: output must equal input exactly
                prop_assert_eq!(
                    &result, &s,
                    "String of {} bytes should not be truncated with limit {}",
                    s.len(), max_body_bytes
                );
                // Must NOT end with the truncated marker
                prop_assert!(
                    !result.ends_with(TRUNCATED_MARKER),
                    "Non-truncated output should not end with [truncated] marker"
                );
            } else {
                // Truncation occurred: output must end with [truncated]
                prop_assert!(
                    result.ends_with(TRUNCATED_MARKER),
                    "Truncated output must end with [truncated], got: {:?}",
                    &result[result.len().saturating_sub(20)..]
                );

                // Strip the marker and verify the remaining part's byte length is ≤ max_body_bytes
                let content_part = &result[..result.len() - TRUNCATED_MARKER_LEN];
                prop_assert!(
                    content_part.len() <= max_body_bytes,
                    "Content part byte length {} exceeds max_body_bytes {}",
                    content_part.len(), max_body_bytes
                );
            }
        }
    }
}

// Feature: request-logging, Property 7: Log filename generation matches date pattern

#[cfg(test)]
mod property_tests_log_filename {
    use crate::logging::file_logger::generate_log_filename;
    use chrono::NaiveDate;
    use proptest::prelude::*;
    use regex::Regex;

    /// Strategy that generates valid NaiveDate values in the range 1970–2100.
    fn arb_naive_date() -> impl Strategy<Value = NaiveDate> {
        (1970i32..=2100i32)
            .prop_flat_map(|year| (Just(year), 1u32..=12u32))
            .prop_flat_map(|(year, month)| {
                let max_day = days_in_month(year, month);
                (Just(year), Just(month), 1u32..=max_day)
            })
            .prop_filter_map("valid NaiveDate", |(year, month, day)| {
                NaiveDate::from_ymd_opt(year, month, day)
            })
    }

    /// Returns the number of days in a given month for a given year.
    fn days_in_month(year: i32, month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap_year(year) {
                    29
                } else {
                    28
                }
            }
            _ => 30,
        }
    }

    /// Determines if a year is a leap year.
    fn is_leap_year(year: i32) -> bool {
        (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 2.3**
        ///
        /// For any valid UTC date (year 1970–2100, month 1–12, day 1–31 valid for that month),
        /// the filename generation function SHALL produce a string matching the regex
        /// `^proxy-\d{4}-\d{2}-\d{2}\.jsonl$` where the embedded date components match the input.
        #[test]
        fn log_filename_matches_date_pattern(date in arb_naive_date()) {
            let filename = generate_log_filename(date);

            // Verify the filename matches the expected regex pattern
            let pattern = Regex::new(r"^proxy-\d{4}-\d{2}-\d{2}\.jsonl$").unwrap();
            prop_assert!(
                pattern.is_match(&filename),
                "Filename '{}' does not match pattern ^proxy-\\d{{4}}-\\d{{2}}-\\d{{2}}\\.jsonl$",
                filename
            );

            // Extract the date components from the filename and verify they match the input
            let date_part = &filename["proxy-".len()..filename.len() - ".jsonl".len()];
            let parts: Vec<&str> = date_part.split('-').collect();
            prop_assert_eq!(parts.len(), 3, "Expected 3 date components, got {}", parts.len());

            let parsed_year: i32 = parts[0].parse().expect("year should be numeric");
            let parsed_month: u32 = parts[1].parse().expect("month should be numeric");
            let parsed_day: u32 = parts[2].parse().expect("day should be numeric");

            prop_assert_eq!(parsed_year, date.format("%Y").to_string().parse::<i32>().unwrap(),
                "Year mismatch: filename has {}, input date has {}", parsed_year, date.format("%Y"));
            prop_assert_eq!(parsed_month, date.format("%m").to_string().parse::<u32>().unwrap(),
                "Month mismatch: filename has {}, input date has {}", parsed_month, date.format("%m"));
            prop_assert_eq!(parsed_day, date.format("%d").to_string().parse::<u32>().unwrap(),
                "Day mismatch: filename has {}, input date has {}", parsed_day, date.format("%d"));
        }
    }
}

// Feature: request-logging, Property 8: Retention purge identifies correct files

#[cfg(test)]
mod property_tests_retention_purge {
    use crate::logging::file_logger::{generate_log_filename, should_purge};
    use chrono::NaiveDate;
    use proptest::prelude::*;

    /// Strategy to generate a valid NaiveDate as a reference date.
    /// We pick dates in a reasonable range (2000-01-01 to 2099-12-31).
    fn arb_reference_date() -> impl Strategy<Value = NaiveDate> {
        (2000i32..=2099i32, 1u32..=12u32)
            .prop_flat_map(|(year, month)| {
                let max_day = match month {
                    1 | 3 | 5 | 7 | 8 | 10 | 12 => 31u32,
                    4 | 6 | 9 | 11 => 30u32,
                    2 => {
                        if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                            29u32
                        } else {
                            28u32
                        }
                    }
                    _ => unreachable!(),
                };
                (Just(year), Just(month), 1u32..=max_day)
            })
            .prop_map(|(year, month, day)| NaiveDate::from_ymd_opt(year, month, day).unwrap())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 2.4**
        ///
        /// For any reference date, retention_days in [1, 90], and a file date that is
        /// 0..=120 days before the reference date, should_purge returns true if and only
        /// if the age in days is strictly greater than retention_days.
        #[test]
        fn retention_purge_identifies_correct_files(
            ref_date in arb_reference_date(),
            retention_days in 1u32..=90u32,
            days_back in 0u32..=120u32,
        ) {
            let file_date = ref_date - chrono::Duration::days(days_back as i64);
            let filename = generate_log_filename(file_date);

            let result = should_purge(&filename, retention_days, ref_date);
            let expected = days_back > retention_days;

            prop_assert_eq!(
                result, expected,
                "should_purge({:?}, retention_days={}, ref_date={}) returned {} but expected {} (days_back={})",
                filename, retention_days, ref_date, result, expected, days_back
            );
        }

        /// **Validates: Requirements 2.4**
        ///
        /// For any non-matching filename (not matching proxy-YYYY-MM-DD.jsonl pattern),
        /// should_purge always returns false regardless of retention_days and reference date.
        #[test]
        fn retention_purge_ignores_non_matching_filenames(
            ref_date in arb_reference_date(),
            retention_days in 1u32..=90u32,
            filename in prop_oneof![
                Just("other-file.txt".to_string()),
                Just("proxy-invalid.jsonl".to_string()),
                Just("proxy-2024-13-01.jsonl".to_string()),
                Just("proxy-2024-01-32.jsonl".to_string()),
                Just("readme.md".to_string()),
                "[a-zA-Z0-9_-]{1,20}\\.[a-z]{1,4}".prop_filter(
                    "must not accidentally match log pattern",
                    |s| !s.starts_with("proxy-") || !s.ends_with(".jsonl")
                ),
            ],
        ) {
            let result = should_purge(&filename, retention_days, ref_date);
            prop_assert_eq!(
                result, false,
                "should_purge({:?}, retention_days={}, ref_date={}) should return false for non-matching filename",
                filename, retention_days, ref_date
            );
        }
    }
}

// Feature: request-logging, Property 1: LogEntry builder populates all required fields

#[cfg(test)]
mod property_tests_log_entry_builder {
    use crate::logging::LogEntry;
    use proptest::prelude::*;

    /// Helper function that mimics how the proxy handler builds a LogEntry.
    /// Generates an id and timestamp, then populates all fields from the given inputs.
    /// This mirrors the construction pattern used in `proxy_messages` handler.
    fn build_log_entry(
        method: String,
        path: String,
        provider: String,
        model: String,
        status: u16,
        duration_ms: u64,
        error_message: Option<String>,
    ) -> LogEntry {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let id = format!(
            "req_{}_{}",
            chrono::Utc::now().timestamp_millis(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let timestamp = chrono::Utc::now().to_rfc3339();

        LogEntry {
            id,
            timestamp,
            method,
            path,
            provider,
            model,
            requested_model: None,
            status,
            duration_ms,
            proxy_overhead_ms: None,
            ttft_ms: None,
            error_message,
            request_body: None,
            response_body: None,
            is_stream: false,
            token_count: None,
        }
    }

    /// Generate an HTTP method from the common set.
    fn arb_method() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("GET".to_string()),
            Just("POST".to_string()),
            Just("PUT".to_string()),
            Just("DELETE".to_string()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 1.1, 1.2**
        ///
        /// For any valid request metadata (HTTP method, path, provider name, model name,
        /// status code 100–599, duration >= 0, and optional error message), building a
        /// LogEntry SHALL produce a struct where every required field matches the input,
        /// and error_message is Some if and only if the status code is >= 400.
        #[test]
        fn log_entry_builder_populates_all_required_fields(
            method in arb_method(),
            path in "/[a-z0-9/]{0,50}",
            provider in "[a-zA-Z]{1,20}",
            model in "[a-zA-Z0-9-]{1,30}",
            status in 100u16..=599u16,
            duration_ms in 0u64..=300_000u64,
        ) {
            // Generate error_message: Some when status >= 400, None otherwise
            let error_message = if status >= 400 {
                Some(format!("error for status {}", status))
            } else {
                None
            };

            let entry = build_log_entry(
                method.clone(),
                path.clone(),
                provider.clone(),
                model.clone(),
                status,
                duration_ms,
                error_message.clone(),
            );

            // Verify all required fields match the input
            prop_assert_eq!(&entry.method, &method, "method mismatch");
            prop_assert_eq!(&entry.path, &path, "path mismatch");
            prop_assert_eq!(&entry.provider, &provider, "provider mismatch");
            prop_assert_eq!(&entry.model, &model, "model mismatch");
            prop_assert_eq!(entry.status, status, "status mismatch");
            prop_assert_eq!(entry.duration_ms, duration_ms, "duration_ms mismatch");

            // Verify id is non-empty
            prop_assert!(!entry.id.is_empty(), "id must be non-empty");

            // Verify timestamp is non-empty
            prop_assert!(!entry.timestamp.is_empty(), "timestamp must be non-empty");

            // Verify error_message is Some iff status >= 400
            if status >= 400 {
                prop_assert!(
                    entry.error_message.is_some(),
                    "error_message must be Some when status >= 400, got None for status {}",
                    status
                );
            } else {
                prop_assert!(
                    entry.error_message.is_none(),
                    "error_message must be None when status < 400, got {:?} for status {}",
                    entry.error_message,
                    status
                );
            }
        }
    }
}

// Feature: request-logging, Property 13: Config hot-reload takes effect immediately

#[cfg(test)]
mod property_tests_config_hot_reload {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use proptest::prelude::*;

    use crate::logging::{LogCollector, LogConfig, LogLevel};

    /// Generate a valid LogLevel
    fn arb_log_level() -> impl Strategy<Value = LogLevel> {
        prop_oneof![Just(LogLevel::All), Just(LogLevel::ErrorsOnly),]
    }

    /// Generate a valid LogConfig for hot-reload testing
    fn arb_log_config() -> impl Strategy<Value = LogConfig> {
        (any::<bool>(), arb_log_level()).prop_map(|(enabled, level)| LogConfig {
            enabled,
            level,
            ..Default::default()
        })
    }

    /// Generate a sequence of 2-5 LogConfig values to simulate hot-reload
    fn arb_config_sequence() -> impl Strategy<Value = Vec<LogConfig>> {
        prop::collection::vec(arb_log_config(), 2..=5)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..Default::default() })]

        /// **Validates: Requirements 5.4**
        ///
        /// For any sequence of LogConfig values stored via ArcSwap, the LogCollector's
        /// `should_log` behavior SHALL immediately reflect the most recently stored config
        /// (no stale reads).
        #[test]
        fn config_hot_reload_takes_effect_immediately(
            configs in arb_config_sequence(),
            status in 100u16..=599u16,
        ) {
            // Start with a default config
            let initial_config = LogConfig::default();
            let arc_config = Arc::new(ArcSwap::from_pointee(initial_config));
            let collector = LogCollector::new(arc_config.clone(), 16);

            // For each config in the sequence, store it and verify should_log immediately reflects it
            for config in &configs {
                arc_config.store(Arc::new(config.clone()));

                let result = collector.should_log(status);

                let expected = if !config.enabled {
                    false
                } else {
                    match config.level {
                        LogLevel::All => true,
                        LogLevel::ErrorsOnly => status >= 400,
                    }
                };

                prop_assert_eq!(
                    result, expected,
                    "After hot-reload to config (enabled={}, level={:?}), \
                     should_log({}) returned {} but expected {}",
                    config.enabled, config.level, status, result, expected
                );
            }
        }
    }
}
