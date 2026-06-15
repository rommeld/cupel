use std::{collections::HashMap, sync::Arc};

use crate::{
    error::{InferenceError, Result},
    model::{ApiFamily, ModelRef, ModelSpec},
    provider::InferenceProvider,
};

#[derive(Debug, Default, Clone)]
pub struct ModelRegistry {
    models: HashMap<ModelRef, ModelSpec>,
}

impl ModelRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, model: ModelSpec) -> Option<ModelSpec> {
        self.models.insert(model.model_ref.clone(), model)
    }

    pub fn get(&self, model_ref: &ModelRef) -> Result<ModelSpec> {
        self.models
            .get(model_ref)
            .cloned()
            .ok_or_else(|| InferenceError::ModelRefNotFound {
                model_ref: model_ref.0.clone(),
            })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ApiFamily, Arc<dyn InferenceProvider>>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        api_family: ApiFamily,
        provider: Arc<dyn InferenceProvider>,
    ) -> Option<Arc<dyn InferenceProvider>> {
        self.providers.insert(api_family, provider)
    }

    pub fn get(&self, api_family: &ApiFamily) -> Result<Arc<dyn InferenceProvider>> {
        self.providers
            .get(api_family)
            .cloned()
            .ok_or_else(|| InferenceError::NoApiProvider(api_family.to_string()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}
