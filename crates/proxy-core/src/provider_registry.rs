use std::collections::HashMap;

use crate::config::{ConfigError, ProviderConfig};

/// Provider 注册表，提供按 name 快速查找
#[derive(Debug)]
pub struct ProviderRegistry {
    providers: Vec<ProviderConfig>,
    index: HashMap<String, usize>,
}

impl ProviderRegistry {
    /// Build a new registry from a list of providers.
    ///
    /// Constructs an index mapping each provider's name to its position
    /// in the vec for O(1) lookup. Returns an error if duplicate names
    /// are found.
    pub fn new(providers: Vec<ProviderConfig>) -> Result<Self, ConfigError> {
        let mut index = HashMap::with_capacity(providers.len());
        for (i, provider) in providers.iter().enumerate() {
            if index.contains_key(&provider.name) {
                return Err(ConfigError::DuplicateName(provider.name.clone()));
            }
            index.insert(provider.name.clone(), i);
        }
        Ok(Self { providers, index })
    }

    /// Look up a provider by name.
    pub fn get(&self, name: &str) -> Option<&ProviderConfig> {
        self.index.get(name).map(|&i| &self.providers[i])
    }

    /// Return a slice of all registered providers.
    pub fn list(&self) -> &[ProviderConfig] {
        &self.providers
    }

    /// Check whether a provider with the given name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Return the number of registered providers.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Return whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
#[path = "provider_registry_tests.rs"]
mod property_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderFormat, ProviderQuirks};

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

    #[test]
    fn new_builds_index_correctly() {
        let providers = vec![
            make_provider("alpha"),
            make_provider("beta"),
            make_provider("gamma"),
        ];
        let registry = ProviderRegistry::new(providers).unwrap();
        assert_eq!(registry.len(), 3);
        assert!(registry.contains("alpha"));
        assert!(registry.contains("beta"));
        assert!(registry.contains("gamma"));
        assert!(!registry.contains("delta"));
    }

    #[test]
    fn new_rejects_duplicate_names() {
        let providers = vec![make_provider("dup"), make_provider("dup")];
        let err = ProviderRegistry::new(providers).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateName(name) if name == "dup"));
    }

    #[test]
    fn get_returns_correct_provider() {
        let providers = vec![make_provider("first"), make_provider("second")];
        let registry = ProviderRegistry::new(providers).unwrap();
        let p = registry.get("second").unwrap();
        assert_eq!(p.name, "second");
    }

    #[test]
    fn get_returns_none_for_missing() {
        let registry = ProviderRegistry::new(vec![make_provider("only")]).unwrap();
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn list_returns_all_providers() {
        let providers = vec![make_provider("a"), make_provider("b")];
        let registry = ProviderRegistry::new(providers).unwrap();
        let list = registry.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "a");
        assert_eq!(list[1].name, "b");
    }

    #[test]
    fn empty_registry() {
        let registry = ProviderRegistry::new(vec![]).unwrap();
        assert_eq!(registry.len(), 0);
        assert!(!registry.contains("anything"));
        assert!(registry.list().is_empty());
    }
}
