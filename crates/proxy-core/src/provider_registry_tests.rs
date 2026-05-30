use proptest::prelude::*;

use crate::config::{
    Config, ConfigError, ProviderConfig, ProviderFormat, ProviderQuirks, ServerConfig,
};
use crate::server::AppState;

// ============================================================================
// Property 5: Runtime provider switch correctness
// ============================================================================

/// Helper: Create a valid ProviderConfig with a given name.
fn make_provider(name: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        base_url: "https://example.com".to_string(),
        api_key: "sk-test".to_string(),
        model: "test-model".to_string(),
        format: ProviderFormat::Openai,
        quirks: ProviderQuirks::default(),
        model_routes: Vec::new(),
        kiro_config: None,
    }
}

/// Helper: Build a valid Config with the given provider names.
/// Sets active_provider to the first provider name.
fn build_config(names: &[String]) -> Config {
    let providers: Vec<ProviderConfig> = names.iter().map(|n| make_provider(n)).collect();
    Config {
        server: ServerConfig::default(),
        provider: providers[0].clone(),
        active_provider: Some(providers[0].name.clone()),
        providers,
        model_routes: Vec::new(),
        logging: Default::default(),
        fallback: Default::default(),
    }
}

/// Generate a vector of 2-5 unique provider names.
fn arb_provider_names() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::hash_set("[a-z][a-z0-9_]{1,12}", 2..=5)
        .prop_map(|set| set.into_iter().collect::<Vec<_>>())
}

/// Generate a name guaranteed to NOT be in a given set of names.
fn arb_nonexistent_name() -> impl Strategy<Value = String> {
    // Use a prefix that won't collide with the [a-z][a-z0-9_]{1,12} pattern
    "zzz_nonexistent_[a-z0-9]{3,8}"
}

// Feature: multi-provider-switching, Property 5: Runtime provider switch correctness
proptest! {
    /// Test that switching to an existing provider name succeeds and
    /// current_provider() returns the provider with that name.
    /// Validates: Requirements 3.1, 3.2, 3.5
    #[test]
    fn switch_to_existing_provider_succeeds(
        names in arb_provider_names(),
        target_idx in any::<prop::sample::Index>(),
    ) {
        let config = build_config(&names);
        let state = AppState::new(config);

        // Pick a target provider from the list
        let target_name = &names[target_idx.index(names.len())];

        // Switch should succeed
        let result = state.switch_provider(target_name);
        prop_assert!(
            result.is_ok(),
            "switch_provider('{}') should succeed, got: {:?}",
            target_name,
            result.err()
        );

        // current_provider() should now return the target provider
        let current = state.current_provider();
        prop_assert_eq!(
            &current.name,
            target_name,
            "current_provider().name should be '{}', got '{}'",
            target_name,
            current.name
        );
    }

    /// Test that switching to a non-existent provider name fails with
    /// ProviderNotFound error containing the invalid name, and
    /// current_provider() remains unchanged.
    /// Validates: Requirements 3.1, 3.2, 3.5
    #[test]
    fn switch_to_nonexistent_provider_fails_and_current_unchanged(
        names in arb_provider_names(),
        nonexistent in arb_nonexistent_name(),
    ) {
        // Ensure the nonexistent name is truly not in the list
        prop_assume!(!names.contains(&nonexistent));

        let config = build_config(&names);
        let state = AppState::new(config);

        // Record the current provider before the failed switch
        let original_name = {
            let current = state.current_provider();
            current.name.clone()
        };

        // Switch to non-existent name should fail
        let result = state.switch_provider(&nonexistent);
        prop_assert!(
            result.is_err(),
            "switch_provider('{}') should fail for non-existent name",
            nonexistent
        );

        // Error should be ProviderNotFound containing the invalid name
        let err = result.unwrap_err();
        match &err {
            ConfigError::ProviderNotFound(name) => {
                prop_assert_eq!(
                    name,
                    &nonexistent,
                    "ProviderNotFound should contain '{}', got '{}'",
                    nonexistent,
                    name
                );
            }
            other => {
                prop_assert!(
                    false,
                    "Expected ProviderNotFound error, got: {:?}",
                    other
                );
            }
        }

        // The error message should contain the invalid name
        let err_msg = err.to_string();
        prop_assert!(
            err_msg.contains(&nonexistent),
            "Error message '{}' should contain the invalid name '{}'",
            err_msg,
            nonexistent
        );

        // current_provider() should remain unchanged
        let current_after = state.current_provider();
        prop_assert_eq!(
            &current_after.name,
            &original_name,
            "current_provider() should remain '{}' after failed switch, got '{}'",
            original_name,
            current_after.name
        );
    }
}
