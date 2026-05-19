use rise_resource_api::{
    is_reserved_collection_name, validate_collection_name, validate_controller_id,
    validate_resource_group, validate_resource_kind, validate_resource_version, OrganizationSpec,
    OrganizationStatus, ResourceDefinitionSpec, ResourceDefinitionStatus,
};

use crate::error::StoreError;

pub trait SpecValidator: Send + Sync {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), StoreError>;
    fn validate_status(&self, status: &serde_json::Value) -> Result<(), StoreError>;
}

pub struct OrganizationValidator;

impl SpecValidator for OrganizationValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), StoreError> {
        let parsed: OrganizationSpec = serde_json::from_value(spec.clone())
            .map_err(|e| StoreError::Validation(format!("invalid Organization spec: {e}")))?;
        if parsed.display_name.trim().is_empty() {
            return Err(StoreError::Validation(
                "Organization spec.displayName must not be empty".into(),
            ));
        }
        Ok(())
    }

    fn validate_status(&self, status: &serde_json::Value) -> Result<(), StoreError> {
        serde_json::from_value::<OrganizationStatus>(status.clone())
            .map_err(|e| StoreError::Validation(format!("invalid Organization status: {e}")))?;
        Ok(())
    }
}

pub struct ResourceDefinitionValidator;

impl SpecValidator for ResourceDefinitionValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), StoreError> {
        let parsed: ResourceDefinitionSpec = serde_json::from_value(spec.clone())
            .map_err(|e| StoreError::Validation(format!("invalid ResourceDefinition spec: {e}")))?;

        validate_resource_group(&parsed.group)
            .map_err(|e| StoreError::Validation(e.to_string()))?;
        validate_resource_kind(&parsed.kind).map_err(|e| StoreError::Validation(e.to_string()))?;
        validate_collection_name(&parsed.plural)
            .map_err(|e| StoreError::Validation(e.to_string()))?;

        if is_reserved_collection_name(&parsed.plural) {
            return Err(StoreError::Validation(format!(
                "plural '{}' is a reserved collection name",
                parsed.plural
            )));
        }

        if parsed.versions.is_empty() {
            return Err(StoreError::Validation(
                "ResourceDefinition must have at least one version".into(),
            ));
        }
        let storage_count = parsed.versions.iter().filter(|v| v.storage).count();
        if storage_count != 1 {
            return Err(StoreError::Validation(
                "ResourceDefinition must have exactly one storage version".into(),
            ));
        }
        for v in &parsed.versions {
            validate_resource_version(&v.name)
                .map_err(|e| StoreError::Validation(e.to_string()))?;
        }

        for id in &parsed.allowed_status_controller_ids {
            validate_controller_id(id).map_err(|e| StoreError::Validation(e.to_string()))?;
        }

        Ok(())
    }

    fn validate_status(&self, status: &serde_json::Value) -> Result<(), StoreError> {
        serde_json::from_value::<ResourceDefinitionStatus>(status.clone()).map_err(|e| {
            StoreError::Validation(format!("invalid ResourceDefinition status: {e}"))
        })?;
        Ok(())
    }
}

pub struct JsonSchemaValidator {
    validator: jsonschema::Validator,
}

impl JsonSchemaValidator {
    pub fn new(schema: serde_json::Value) -> Result<Self, StoreError> {
        let validator = jsonschema::validator_for(&schema).map_err(|e| {
            StoreError::Validation(format!("invalid JSON schema in ResourceDefinition: {e}"))
        })?;
        Ok(Self { validator })
    }
}

impl SpecValidator for JsonSchemaValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), StoreError> {
        let errors: Vec<String> = self
            .validator
            .iter_errors(spec)
            .map(|e| e.to_string())
            .collect();
        if !errors.is_empty() {
            return Err(StoreError::Validation(format!(
                "spec validation failed: {}",
                errors.join("; ")
            )));
        }
        Ok(())
    }

    fn validate_status(&self, _status: &serde_json::Value) -> Result<(), StoreError> {
        Ok(())
    }
}

pub struct NoOpValidator;

impl SpecValidator for NoOpValidator {
    fn validate_spec(&self, _spec: &serde_json::Value) -> Result<(), StoreError> {
        Ok(())
    }
    fn validate_status(&self, _status: &serde_json::Value) -> Result<(), StoreError> {
        Ok(())
    }
}
