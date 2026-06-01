use std::{collections::HashMap, sync::Arc};

use crate::{ApiFamily, InferenceError, ModelRef, ModelSpec, provider::InferenceProvider};

#[derive(Default, Clone)]
pub struct ModelRegistry {
    models: HashMap<ModelRef, ModelSpec>,
}

impl ModelRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    pub fn insert(&mut self, model: ModelSpec) {
        self.models.insert(model.model_ref.clone(), model);
    }

    /// Returns the model specification registered for `model_ref`.
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError::ModelNotFound`] when `model_ref` has not been registered.
    pub fn get(&self, model_ref: &ModelRef) -> Result<ModelSpec, InferenceError> {
        self.models
            .get(model_ref)
            .cloned()
            .ok_or_else(|| InferenceError::ModelNotFound(model_ref.clone()))
    }
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ApiFamily, Arc<dyn InferenceProvider>>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, api_family: ApiFamily, provider: Arc<dyn InferenceProvider>) {
        self.providers.insert(api_family, provider);
    }

    /// Returns the inference provider registered for `api_family`.
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError::ProviderNotRegistered`] when `api_family` has not been registered.
    pub fn get(
        &self,
        api_family: &ApiFamily,
    ) -> Result<Arc<dyn InferenceProvider>, InferenceError> {
        self.providers
            .get(api_family)
            .cloned()
            .ok_or_else(|| InferenceError::ProviderNotRegistered(api_family.clone()))
    }
}
